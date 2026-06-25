//! Daemon-owned asset storage and attachment rendering helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use image::{
    DynamicImage, GenericImageView, ImageFormat, ImageReader, Limits, Rgb, RgbImage,
    codecs::jpeg::JpegEncoder, imageops::FilterType,
};
use lopdf::{Document as PdfDocument, Object as PdfObject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use kheish_types::{
    AttachmentRef, DEFAULT_DOCUMENT_ATTACHMENT_TEXT_CHAR_LIMIT, asset_storage_uri,
    parse_asset_storage_uri, render_document_attachment_text,
};

use crate::state_files::read_json_or_quarantine;

pub(crate) const MAX_ASSET_BYTES: usize = 12 * 1024 * 1024;
const MAX_IMAGE_EDGE_PX: u32 = 2048;
const MAX_SOURCE_IMAGE_EDGE_PX: u32 = 16_384;
const MAX_SOURCE_IMAGE_PIXELS: u64 = 50_000_000;
const MAX_IMAGE_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NORMALIZED_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const DXF_PREVIEW_MAX_EDGE_PX: u32 = 1024;
const DXF_PREVIEW_PADDING_PX: u32 = 24;
const DXF_PREVIEW_BACKGROUND: [u8; 3] = [255, 255, 255];
const DXF_PREVIEW_FOREGROUND: [u8; 3] = [0, 0, 0];
const DXF_PREVIEW_LINE_THICKNESS_PX: i32 = 2;
const MAX_DXF_PREVIEW_PRIMITIVES: usize = 20_000;
const MAX_DXF_PREVIEW_SEGMENTS: usize = 20_000;
const MAX_DXF_POLYLINE_POINTS: usize = 20_000;
const MAX_PDF_TEXT_PAGES: usize = 128;
const MAX_PDF_OBJECTS: usize = 10_000;
const MAX_PDF_STREAMS: usize = 2_048;
const MAX_PDF_STREAM_BYTES: usize = MAX_ASSET_BYTES;
const MAX_PDF_TOTAL_STREAM_BYTES: usize = MAX_ASSET_BYTES;
const MAX_PDF_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MIN_AUDIO_SAMPLE_RATE_HZ: u32 = 8_000;
const MAX_AUDIO_SAMPLE_RATE_HZ: u32 = 192_000;
const MAX_AUDIO_CHANNELS: u16 = 8;
const MAX_AUDIO_DURATION_MS: u64 = 30 * 60 * 1000;
const MAX_ASSET_STARTUP_REPAIR_DIAGNOSTICS: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AssetStartupRepairReport {
    pub repaired_count: usize,
    pub skipped_asset_count: usize,
    pub skipped_raw_missing_count: usize,
    pub skipped_raw_integrity_mismatch_count: usize,
    pub invalid_tombstone_count: usize,
    pub completed_tombstone_delete_count: usize,
    pub restored_derived_text_count: usize,
    pub restored_derived_preview_count: usize,
    pub integrity_backfilled_count: usize,
    pub diagnostics: Vec<AssetStartupRepairDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AssetStartupRepairDiagnostic {
    pub asset_id: Option<String>,
    pub kind: String,
    pub action: String,
    pub reason: String,
    pub uri: Option<String>,
}

impl AssetStartupRepairReport {
    fn record(
        &mut self,
        asset_id: Option<&str>,
        kind: &str,
        action: &str,
        reason: &str,
        uri: Option<&str>,
    ) {
        if self.diagnostics.len() >= MAX_ASSET_STARTUP_REPAIR_DIAGNOSTICS {
            return;
        }
        self.diagnostics.push(AssetStartupRepairDiagnostic {
            asset_id: asset_id.map(ToOwned::to_owned),
            kind: kind.to_string(),
            action: action.to_string(),
            reason: reason.to_string(),
            uri: uri.map(ToOwned::to_owned),
        });
    }
}

/// Stored metadata for one daemon-owned asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredAssetRecord {
    /// The stable asset identifier.
    pub id: String,
    /// The normalized MIME type.
    pub media_type: String,
    /// The original file name provided by the caller.
    pub file_name: String,
    /// The normalized SHA-256 digest of the raw payload.
    pub sha256: String,
    /// The raw payload size in bytes.
    pub byte_length: u64,
    /// The daemon-managed raw storage URI.
    pub uri: String,
    /// The daemon-managed derived text storage URI when the asset is renderable as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_uri: Option<String>,
    /// The normalized SHA-256 digest of the derived text payload when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_sha256: Option<String>,
    /// The derived text payload size in bytes when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_byte_length: Option<u64>,
    /// The daemon-managed visual preview storage URI when one is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_uri: Option<String>,
    /// The normalized MIME type used for the derived visual preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_media_type: Option<String>,
    /// The normalized SHA-256 digest of the derived visual preview payload when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_sha256: Option<String>,
    /// The derived visual preview payload size in bytes when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_image_byte_length: Option<u64>,
    /// Derivations whose result points at this asset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derivation_ids: Vec<String>,
    /// Durable provenance records for daemon-produced assets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<AssetProvenanceRecord>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
}

/// One compact source asset entry recorded in durable asset provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AssetProvenanceSourceRecord {
    /// The source daemon asset identifier.
    pub asset_id: String,
    /// The source asset media type at dispatch time.
    pub media_type: String,
    /// The source asset raw SHA-256 at dispatch time.
    pub sha256: String,
}

/// One durable provenance record attached to a daemon-owned asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AssetProvenanceRecord {
    /// Provider-neutral producer kind, such as `image_generation`, `image_edit`, or `audio_generation`.
    pub kind: String,
    /// Tool name that produced the asset.
    pub tool_name: String,
    /// Session that requested the asset when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Run that requested the asset when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Model tool-call id that requested the asset when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Selected daemon media route id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// Backend provider that produced the bytes.
    pub provider: String,
    /// Concrete backend model that produced the bytes.
    pub model: String,
    /// SHA-256 of the prompt/instruction text; raw prompt text is intentionally not stored here.
    pub prompt_sha256: String,
    /// Ordered source assets used by edit-style producers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_assets: Vec<AssetProvenanceSourceRecord>,
    /// 1-based index of this output within the provider batch.
    pub output_index: u32,
    /// Total number of outputs in the provider batch.
    pub output_count: u32,
}

/// Durable marker for an asset identifier that was physically removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AssetTombstoneRecord {
    /// The stable asset identifier that must not be reused.
    pub asset_id: String,
    /// The normalized MIME type the asset had before deletion.
    pub media_type: String,
    /// The original file name recorded for the asset.
    pub file_name: String,
    /// The normalized SHA-256 digest of the removed raw payload.
    pub sha256: String,
    /// The removed raw payload size in bytes.
    pub byte_length: u64,
    /// The deletion timestamp in milliseconds since the Unix epoch.
    pub deleted_at_ms: u64,
    /// The caller-visible reason for the deletion.
    pub reason: String,
}

/// One daemon-owned file that would be removed when an asset is physically deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AssetDeletionFileRecord {
    /// Logical file role, such as `raw`, `derived_text`, `preview`, or `metadata`.
    pub kind: String,
    /// Opaque daemon storage URI for payload files. Metadata files do not have one.
    pub uri: Option<String>,
    /// Current on-disk byte length, or zero when the file is already missing.
    pub byte_length: u64,
    /// Whether the file exists at planning time.
    pub exists: bool,
}

impl StoredAssetRecord {
    /// Returns the provider/runtime attachment reference persisted in session journals.
    pub(crate) fn attachment_ref(&self) -> AttachmentRef {
        AttachmentRef {
            id: self.id.clone(),
            media_type: self.media_type.clone(),
            uri: self.uri.clone(),
            file_name: Some(self.file_name.clone()),
            sha256: Some(self.sha256.clone()),
            byte_length: Some(self.byte_length),
            text_uri: self.text_uri.clone(),
            text_sha256: self.text_sha256.clone(),
            text_byte_length: self.text_byte_length,
            preview_image_uri: self.preview_image_uri.clone(),
            preview_image_media_type: self.preview_image_media_type.clone(),
            preview_image_sha256: self.preview_image_sha256.clone(),
            preview_image_byte_length: self.preview_image_byte_length,
        }
    }

    /// Returns whether this asset should be treated as a true multimodal image.
    pub(crate) fn is_image(&self) -> bool {
        matches!(self.media_type.as_str(), "image/png" | "image/jpeg")
    }
}

#[derive(Default)]
struct AssetCatalog {
    by_id: BTreeMap<String, StoredAssetRecord>,
    by_digest: BTreeMap<String, String>,
    by_uri: BTreeMap<String, String>,
    inflight_by_digest: BTreeMap<String, Arc<PendingImport>>,
}

struct PendingImport {
    state: StdMutex<PendingImportState>,
    ready: Condvar,
}

#[derive(Default)]
struct PendingImportState {
    settled: bool,
    record: Option<StoredAssetRecord>,
    error: Option<String>,
}

impl PendingImport {
    fn new() -> Self {
        Self {
            state: StdMutex::new(PendingImportState::default()),
            ready: Condvar::new(),
        }
    }
}

enum ImportReservation {
    Existing(StoredAssetRecord),
    Wait(Arc<PendingImport>),
    Owner {
        id: String,
        pending: Arc<PendingImport>,
    },
}

/// A durable file-backed asset store rooted under the daemon state directory.
pub(crate) struct FileAssetStore {
    root: PathBuf,
    catalog: StdMutex<AssetCatalog>,
    startup_repair_report: AssetStartupRepairReport,
    next_id: AtomicU64,
}

impl FileAssetStore {
    /// Loads the asset store rooted under the provided state directory.
    pub(crate) fn new(state_root: impl Into<PathBuf>) -> Result<Self> {
        let root = state_root.into().join("assets");
        fs::create_dir_all(root.join("meta"))?;
        fs::create_dir_all(root.join("raw"))?;
        fs::create_dir_all(root.join("text"))?;
        fs::create_dir_all(root.join("preview"))?;
        fs::create_dir_all(root.join("tombstones"))?;
        cleanup_asset_atomic_temp_files(&root)?;
        let mut catalog = AssetCatalog::default();
        let mut max_suffix = 0u64;
        let mut startup_repair_report = AssetStartupRepairReport::default();
        let (tombstoned_asset_ids, tombstone_max_suffix) =
            load_tombstoned_asset_ids(&root, &mut startup_repair_report)?;
        max_suffix = max_suffix.max(tombstone_max_suffix);
        complete_tombstoned_asset_deletions(
            &root,
            &tombstoned_asset_ids,
            &mut startup_repair_report,
        )?;
        for entry in fs::read_dir(root.join("meta"))? {
            let entry = entry?;
            let path = entry.path();
            if let Some(suffix) = asset_suffix_from_metadata_path(&path) {
                max_suffix = max_suffix.max(suffix);
            }
            let Some(meta_asset_id) = asset_id_from_metadata_path(&path) else {
                continue;
            };
            if tombstoned_asset_ids.contains(&meta_asset_id) {
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(record) =
                read_json_or_quarantine::<StoredAssetRecord>(&path, "asset metadata")?
            else {
                continue;
            };
            let Some(record) =
                validate_loaded_asset_record(&root, &path, record, &mut startup_repair_report)?
            else {
                continue;
            };
            if let Some(suffix) = asset_suffix_from_id(&record.id) {
                max_suffix = max_suffix.max(suffix);
            }
            let digest_key = digest_key(&record.media_type, &record.sha256);
            catalog.by_digest.insert(digest_key, record.id.clone());
            catalog.by_uri.insert(record.uri.clone(), record.id.clone());
            catalog.by_id.insert(record.id.clone(), record);
        }
        Ok(Self {
            root,
            catalog: StdMutex::new(catalog),
            startup_repair_report,
            next_id: AtomicU64::new(max_suffix.saturating_add(1)),
        })
    }

    pub(crate) fn startup_repair_report(&self) -> AssetStartupRepairReport {
        self.startup_repair_report.clone()
    }

    /// Imports one inline payload into the daemon-owned store or returns the deduplicated asset.
    pub(crate) fn import_bytes(
        &self,
        file_name: &str,
        declared_media_type: Option<&str>,
        bytes: &[u8],
    ) -> Result<StoredAssetRecord> {
        self.import_bytes_with_provenance(file_name, declared_media_type, bytes, None)
    }

    /// Validates and normalizes the media type that would be assigned during import.
    pub(crate) fn validate_import_media_type(
        &self,
        file_name: &str,
        declared_media_type: Option<&str>,
        bytes: &[u8],
    ) -> Result<String> {
        normalize_media_type(file_name, declared_media_type, bytes)
    }

    /// Imports one inline payload and attaches optional daemon-owned provenance.
    pub(crate) fn import_bytes_with_provenance(
        &self,
        file_name: &str,
        declared_media_type: Option<&str>,
        bytes: &[u8],
        provenance: Option<AssetProvenanceRecord>,
    ) -> Result<StoredAssetRecord> {
        if bytes.is_empty() {
            bail!("asset payload is empty");
        }
        if bytes.len() > MAX_ASSET_BYTES {
            bail!("asset payload exceeds the {} byte limit", MAX_ASSET_BYTES);
        }
        let media_type = normalize_media_type(file_name, declared_media_type, bytes)?;
        let stored_bytes = prepare_stored_payload(&media_type, bytes)?;
        let sha256 = hex::encode(Sha256::digest(&stored_bytes));
        let digest_key = digest_key(&media_type, &sha256);
        loop {
            match self.reserve_import(&digest_key) {
                ImportReservation::Existing(existing) => {
                    return self.append_asset_provenance_if_needed(existing, provenance.clone());
                }
                ImportReservation::Wait(pending) => {
                    let mut state = pending
                        .state
                        .lock()
                        .expect("asset import reservation mutex poisoned");
                    while !state.settled {
                        state = pending
                            .ready
                            .wait(state)
                            .expect("asset import reservation wait poisoned");
                    }
                    if let Some(record) = state.record.clone() {
                        return self.append_asset_provenance_if_needed(record, provenance.clone());
                    }
                    if let Some(message) = state.error.clone() {
                        bail!("{message}");
                    }
                    bail!("asset import reservation settled without a result");
                }
                ImportReservation::Owner { id, pending } => {
                    return self.finish_reserved_import(
                        file_name,
                        media_type,
                        stored_bytes,
                        sha256,
                        digest_key,
                        id,
                        pending,
                        provenance,
                    );
                }
            }
        }
    }

    /// Returns one stored asset by identifier when it exists.
    pub(crate) fn get(&self, asset_id: &str) -> Option<StoredAssetRecord> {
        self.catalog
            .lock()
            .expect("asset catalog mutex poisoned")
            .by_id
            .get(asset_id)
            .cloned()
    }

    /// Returns one stored asset by raw storage URI when it exists.
    pub(crate) fn get_by_uri(&self, uri: &str) -> Option<StoredAssetRecord> {
        let catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
        let asset_id = catalog.by_uri.get(uri)?;
        catalog.by_id.get(asset_id).cloned()
    }

    /// Reads one stored asset record together with its raw payload bytes.
    pub(crate) fn read_raw(&self, asset_id: &str) -> Result<(StoredAssetRecord, Vec<u8>)> {
        let record = self
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        let path = asset_path_from_uri(&self.root, &record.uri)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "asset integrity mismatch for {}: raw payload is missing",
                    record.id
                );
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read raw asset payload {}", path.display())
                });
            }
        };
        validate_raw_asset_bytes(&record, &bytes)?;
        Ok((record, bytes))
    }

    /// Reads the full derived text payload for one asset when the asset exposes one.
    pub(crate) fn read_text(&self, asset_id: &str) -> Result<Option<String>> {
        let record = self
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        let Some(text_uri) = record.text_uri.as_ref() else {
            return Ok(None);
        };
        let path = asset_path_from_uri(&self.root, text_uri)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read derived asset text {}", path.display()))?;
        validate_derived_asset_bytes(
            &record.id,
            "derived text",
            record.text_sha256.as_deref(),
            record.text_byte_length,
            &bytes,
        )?;
        let text = String::from_utf8(bytes)
            .with_context(|| format!("derived asset text is not UTF-8 for {}", record.id))?;
        Ok(Some(text))
    }

    /// Reads the derived preview image payload for one asset when the asset exposes one.
    pub(crate) fn read_preview_image(&self, asset_id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let record = self
            .get(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        let Some(preview_uri) = record.preview_image_uri.as_ref() else {
            return Ok(None);
        };
        let media_type = record
            .preview_image_media_type
            .clone()
            .ok_or_else(|| anyhow!("asset {asset_id} is missing preview_image_media_type"))?;
        let path = asset_path_from_uri(&self.root, preview_uri)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read derived asset preview {}", path.display()))?;
        validate_derived_asset_bytes(
            &record.id,
            "derived preview",
            record.preview_image_sha256.as_deref(),
            record.preview_image_byte_length,
            &bytes,
        )?;
        validate_media_type(&media_type, &bytes)
            .with_context(|| format!("asset {} has an invalid derived preview", record.id))?;
        Ok(Some((media_type, bytes)))
    }

    /// Returns all stored assets optionally filtered by a free-text query.
    pub(crate) fn list(&self, query: Option<&str>) -> Vec<StoredAssetRecord> {
        let normalized = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        self.catalog
            .lock()
            .expect("asset catalog mutex poisoned")
            .by_id
            .values()
            .filter(|record| {
                normalized.as_ref().is_none_or(|query| {
                    record.id.to_ascii_lowercase().contains(query)
                        || record.file_name.to_ascii_lowercase().contains(query)
                        || record.media_type.to_ascii_lowercase().contains(query)
                        || record.sha256.to_ascii_lowercase().contains(query)
                        || record
                            .derivation_ids
                            .iter()
                            .any(|derivation_id| derivation_id.to_ascii_lowercase().contains(query))
                })
            })
            .cloned()
            .collect()
    }

    /// Renders one bounded transcript fragment for a single attachment inside canonical history.
    pub(crate) fn render_asset_transcript_part(&self, asset: &StoredAssetRecord) -> Result<String> {
        if asset.is_image() {
            return Ok(format!(
                "Attached image: {} ({})",
                asset.file_name, asset.media_type
            ));
        }
        if asset.text_uri.is_none() {
            return Ok(format!(
                "Attached file: {} ({})",
                asset.file_name, asset.media_type
            ));
        };
        let raw = self.read_text(&asset.id)?.unwrap_or_default();
        Ok(render_document_attachment_text(
            &asset.file_name,
            &asset.media_type,
            &raw,
            DEFAULT_DOCUMENT_ATTACHMENT_TEXT_CHAR_LIMIT,
        )
        .trim()
        .to_string())
    }

    /// Attaches one existing text/plain asset as the canonical text representation of another asset.
    pub(crate) fn attach_text_asset(
        &self,
        asset_id: &str,
        text_asset: &StoredAssetRecord,
    ) -> Result<StoredAssetRecord> {
        anyhow::ensure!(
            text_asset.media_type == "text/plain",
            "text asset {} must be text/plain",
            text_asset.id
        );
        let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
        let record = catalog
            .by_id
            .get_mut(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        if record.text_uri.as_deref() == Some(text_asset.uri.as_str())
            && record.text_sha256.as_deref() == Some(text_asset.sha256.as_str())
            && record.text_byte_length == Some(text_asset.byte_length)
        {
            return Ok(record.clone());
        }
        let previous = record.clone();
        record.text_uri = Some(text_asset.uri.clone());
        record.text_sha256 = Some(text_asset.sha256.clone());
        record.text_byte_length = Some(text_asset.byte_length);
        let updated = record.clone();
        let meta_path = self.asset_meta_path(asset_id);
        if let Err(error) = write_asset_metadata_atomic(&meta_path, &updated) {
            *record = previous;
            return Err(error);
        }
        Ok(updated)
    }

    /// Attaches one derivation identifier to an asset result provenance list.
    pub(crate) fn attach_derivation(
        &self,
        asset_id: &str,
        derivation_id: &str,
    ) -> Result<StoredAssetRecord> {
        anyhow::ensure!(
            !derivation_id.trim().is_empty(),
            "derivation_id is required"
        );
        let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
        let record = catalog
            .by_id
            .get_mut(asset_id)
            .ok_or_else(|| anyhow!("unknown asset {asset_id}"))?;
        if record
            .derivation_ids
            .iter()
            .any(|existing| existing == derivation_id)
        {
            return Ok(record.clone());
        }
        let previous = record.clone();
        record.derivation_ids.push(derivation_id.to_string());
        record.derivation_ids.sort();
        let updated = record.clone();
        let meta_path = self.asset_meta_path(asset_id);
        if let Err(error) = write_asset_metadata_atomic(&meta_path, &updated) {
            *record = previous;
            return Err(error);
        }
        Ok(updated)
    }

    /// Removes one asset catalog entry and its daemon-owned payload files when present.
    pub(crate) fn delete_asset(&self, asset_id: &str) -> Result<bool> {
        self.delete_asset_with_reason(asset_id, "delete")
    }

    /// Removes one asset catalog entry and records a durable tombstone for its identifier.
    pub(crate) fn delete_asset_with_reason(&self, asset_id: &str, reason: &str) -> Result<bool> {
        let record = {
            let catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
            let Some(record) = catalog.by_id.get(asset_id).cloned() else {
                return Ok(false);
            };
            record
        };

        write_asset_tombstone_atomic(
            &self.asset_tombstone_path(asset_id),
            &AssetTombstoneRecord {
                asset_id: record.id.clone(),
                media_type: record.media_type.clone(),
                file_name: record.file_name.clone(),
                sha256: record.sha256.clone(),
                byte_length: record.byte_length,
                deleted_at_ms: now_ms(),
                reason: reason.to_string(),
            },
        )?;
        {
            let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
            let Some(removed) = catalog.by_id.remove(asset_id) else {
                return Ok(false);
            };
            catalog
                .by_digest
                .remove(&digest_key(&removed.media_type, &removed.sha256));
            catalog.by_uri.remove(&removed.uri);
        }
        if is_owned_raw_asset_uri(asset_id, &record.uri) {
            remove_asset_file_if_exists(asset_path_from_uri(&self.root, &record.uri)?)?;
        }
        if let Some(text_uri) = record.text_uri.as_deref() {
            if is_owned_derived_asset_uri(asset_id, text_uri, "text") {
                remove_asset_file_if_exists(asset_path_from_uri(&self.root, text_uri)?)?;
            }
        }
        if let Some(preview_uri) = record.preview_image_uri.as_deref() {
            if is_owned_derived_asset_uri(asset_id, preview_uri, "preview") {
                remove_asset_file_if_exists(asset_path_from_uri(&self.root, preview_uri)?)?;
            }
        }
        remove_asset_file_if_exists(self.asset_meta_path(asset_id))?;
        Ok(true)
    }

    /// Returns the daemon-owned files that direct deletion would remove for this asset.
    pub(crate) fn deletion_files(
        &self,
        record: &StoredAssetRecord,
    ) -> Result<Vec<AssetDeletionFileRecord>> {
        let mut files = Vec::new();
        if is_owned_raw_asset_uri(&record.id, &record.uri) {
            files.push(asset_deletion_file_record(
                "raw",
                Some(record.uri.clone()),
                asset_path_from_uri(&self.root, &record.uri)?,
            )?);
        }
        if let Some(text_uri) = record.text_uri.as_deref()
            && is_owned_derived_asset_uri(&record.id, text_uri, "text")
        {
            files.push(asset_deletion_file_record(
                "derived_text",
                Some(text_uri.to_string()),
                asset_path_from_uri(&self.root, text_uri)?,
            )?);
        }
        if let Some(preview_uri) = record.preview_image_uri.as_deref()
            && is_owned_derived_asset_uri(&record.id, preview_uri, "preview")
        {
            files.push(asset_deletion_file_record(
                "preview",
                Some(preview_uri.to_string()),
                asset_path_from_uri(&self.root, preview_uri)?,
            )?);
        }
        files.push(asset_deletion_file_record(
            "metadata",
            None,
            self.asset_meta_path(&record.id),
        )?);
        Ok(files)
    }

    /// Returns payload files in raw/text/preview directories that are not referenced by metadata.
    pub(crate) fn orphan_payload_files(&self) -> Result<Vec<AssetDeletionFileRecord>> {
        let referenced_uris = {
            let catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
            let mut referenced_uris = std::collections::BTreeSet::new();
            for record in catalog.by_id.values() {
                referenced_uris.insert(record.uri.clone());
                if let Some(text_uri) = record.text_uri.clone() {
                    referenced_uris.insert(text_uri);
                }
                if let Some(preview_uri) = record.preview_image_uri.clone() {
                    referenced_uris.insert(preview_uri);
                }
            }
            referenced_uris
        };
        let mut files = Vec::new();
        for kind in ["raw", "text", "preview"] {
            for entry in fs::read_dir(self.root.join(kind))
                .with_context(|| format!("failed to read asset {kind} directory"))?
            {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                if is_asset_atomic_temp_file_name(file_name) {
                    continue;
                }
                if !is_asset_owned_file_name(file_name) {
                    continue;
                }
                let uri = asset_storage_uri(kind, file_name);
                if referenced_uris.contains(&uri) {
                    continue;
                }
                files.push(asset_deletion_file_record(
                    &format!("orphan_{kind}"),
                    Some(uri),
                    entry.path(),
                )?);
            }
        }
        files.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.uri.cmp(&right.uri))
        });
        Ok(files)
    }

    /// Removes one orphan payload file by opaque daemon storage URI.
    pub(crate) fn delete_orphan_payload_file(&self, uri: &str) -> Result<bool> {
        let Some((kind, _)) = parse_asset_storage_uri(uri) else {
            bail!("invalid asset storage uri '{uri}'");
        };
        anyhow::ensure!(
            matches!(kind, "raw" | "text" | "preview"),
            "asset orphan GC only removes payload files"
        );
        if self.get_by_uri(uri).is_some()
            || self.list(None).into_iter().any(|record| {
                record.text_uri.as_deref() == Some(uri)
                    || record.preview_image_uri.as_deref() == Some(uri)
            })
        {
            return Ok(false);
        }
        let path = asset_path_from_uri(&self.root, uri)?;
        let existed = path.is_file();
        remove_asset_file_if_exists(path)?;
        Ok(existed)
    }
}

impl FileAssetStore {
    fn asset_meta_path(&self, asset_id: &str) -> PathBuf {
        self.root.join("meta").join(format!("{asset_id}.json"))
    }

    fn asset_tombstone_path(&self, asset_id: &str) -> PathBuf {
        self.root
            .join("tombstones")
            .join(format!("{asset_id}.json"))
    }

    fn reserve_import(&self, digest_key: &str) -> ImportReservation {
        let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
        if let Some(existing) = catalog
            .by_digest
            .get(digest_key)
            .and_then(|id| catalog.by_id.get(id))
            .cloned()
        {
            return ImportReservation::Existing(existing);
        }
        if let Some(pending) = catalog.inflight_by_digest.get(digest_key).cloned() {
            return ImportReservation::Wait(pending);
        }
        let pending = Arc::new(PendingImport::new());
        catalog
            .inflight_by_digest
            .insert(digest_key.to_string(), pending.clone());
        let id = format!("asset-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        ImportReservation::Owner { id, pending }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_reserved_import(
        &self,
        file_name: &str,
        media_type: String,
        stored_bytes: Vec<u8>,
        sha256: String,
        digest_key: String,
        id: String,
        pending: Arc<PendingImport>,
        provenance: Option<AssetProvenanceRecord>,
    ) -> Result<StoredAssetRecord> {
        let extension = preferred_extension(&media_type);
        let raw_relative_path = format!("{id}.{extension}");
        let raw_uri = asset_storage_uri("raw", &raw_relative_path);
        let raw_path = asset_path_from_uri(&self.root, &raw_uri)?;
        let meta_path = self.asset_meta_path(&id);
        let mut text_disk_path: Option<PathBuf> = None;
        let mut preview_disk_path: Option<PathBuf> = None;

        let result = (|| -> Result<StoredAssetRecord> {
            let derived = derive_asset_artifacts(&media_type, &stored_bytes)?;

            write_file_atomic(&raw_path, &stored_bytes)
                .with_context(|| format!("failed to write asset payload {}", raw_path.display()))?;

            let text_path = if let Some(text) = derived.text {
                let text_uri = asset_storage_uri("text", &format!("{id}.txt"));
                let path = asset_path_from_uri(&self.root, &text_uri)?;
                write_file_atomic(&path, text.as_bytes()).with_context(|| {
                    format!("failed to write derived asset text {}", path.display())
                })?;
                let (text_sha256, text_byte_length) = payload_integrity(text.as_bytes());
                text_disk_path = Some(path.clone());
                Some((text_uri, path, text_sha256, text_byte_length))
            } else {
                None
            };
            let preview_path = if let Some((preview_media_type, preview_bytes)) = derived.preview {
                let preview_extension = preferred_extension(&preview_media_type);
                let preview_uri =
                    asset_storage_uri("preview", &format!("{id}.{preview_extension}"));
                let path = asset_path_from_uri(&self.root, &preview_uri)?;
                write_file_atomic(&path, &preview_bytes).with_context(|| {
                    format!("failed to write derived asset preview {}", path.display())
                })?;
                let (preview_sha256, preview_byte_length) = payload_integrity(&preview_bytes);
                preview_disk_path = Some(path.clone());
                Some((
                    preview_uri,
                    preview_media_type,
                    path,
                    preview_sha256,
                    preview_byte_length,
                ))
            } else {
                None
            };

            let record = StoredAssetRecord {
                id: id.clone(),
                media_type,
                file_name: file_name.to_string(),
                sha256,
                byte_length: stored_bytes.len() as u64,
                uri: raw_uri,
                text_uri: text_path.as_ref().map(|(uri, _, _, _)| uri.clone()),
                text_sha256: text_path.as_ref().map(|(_, _, sha256, _)| sha256.clone()),
                text_byte_length: text_path
                    .as_ref()
                    .map(|(_, _, _, byte_length)| *byte_length),
                preview_image_uri: preview_path.as_ref().map(|(uri, _, _, _, _)| uri.clone()),
                preview_image_media_type: preview_path
                    .as_ref()
                    .map(|(_, media_type, _, _, _)| media_type.clone()),
                preview_image_sha256: preview_path
                    .as_ref()
                    .map(|(_, _, _, sha256, _)| sha256.clone()),
                preview_image_byte_length: preview_path
                    .as_ref()
                    .map(|(_, _, _, _, byte_length)| *byte_length),
                derivation_ids: Vec::new(),
                provenance: provenance.into_iter().collect(),
                created_at_ms: now_ms(),
            };
            write_asset_metadata_atomic(&meta_path, &record)?;
            Ok(record)
        })();

        match result {
            Ok(record) => {
                let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
                catalog.inflight_by_digest.remove(&digest_key);
                catalog.by_digest.insert(digest_key, id);
                catalog.by_uri.insert(record.uri.clone(), record.id.clone());
                catalog.by_id.insert(record.id.clone(), record.clone());
                let mut state = pending
                    .state
                    .lock()
                    .expect("asset import reservation mutex poisoned");
                state.settled = true;
                state.record = Some(record.clone());
                pending.ready.notify_all();
                Ok(record)
            }
            Err(error) => {
                let _ = fs::remove_file(&raw_path);
                if let Some(path) = text_disk_path.as_ref() {
                    let _ = fs::remove_file(path);
                }
                if let Some(path) = preview_disk_path.as_ref() {
                    let _ = fs::remove_file(path);
                }
                let _ = fs::remove_file(&meta_path);

                let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
                catalog.inflight_by_digest.remove(&digest_key);
                drop(catalog);

                let mut state = pending
                    .state
                    .lock()
                    .expect("asset import reservation mutex poisoned");
                state.settled = true;
                state.error = Some(error.to_string());
                pending.ready.notify_all();
                Err(error)
            }
        }
    }

    fn append_asset_provenance_if_needed(
        &self,
        mut record: StoredAssetRecord,
        provenance: Option<AssetProvenanceRecord>,
    ) -> Result<StoredAssetRecord> {
        let Some(provenance) = provenance else {
            return Ok(record);
        };
        let mut catalog = self.catalog.lock().expect("asset catalog mutex poisoned");
        record = catalog.by_id.get(&record.id).cloned().unwrap_or(record);
        if record.provenance.contains(&provenance) {
            return Ok(record);
        }
        record.provenance.push(provenance);
        let meta_path = self.asset_meta_path(&record.id);
        write_asset_metadata_atomic(&meta_path, &record)?;
        catalog.by_id.insert(record.id.clone(), record.clone());
        Ok(record)
    }
}

fn digest_key(media_type: &str, sha256: &str) -> String {
    format!("{media_type}:{sha256}")
}

fn payload_integrity(bytes: &[u8]) -> (String, u64) {
    (hex::encode(Sha256::digest(bytes)), bytes.len() as u64)
}

fn validate_raw_asset_bytes(record: &StoredAssetRecord, bytes: &[u8]) -> Result<()> {
    let (actual_sha256, actual_byte_length) = payload_integrity(bytes);
    anyhow::ensure!(
        actual_sha256 == record.sha256 && actual_byte_length == record.byte_length,
        "asset integrity mismatch for {}: expected sha256 {} and {} bytes, found sha256 {} and {} bytes",
        record.id,
        record.sha256,
        record.byte_length,
        actual_sha256,
        actual_byte_length
    );
    Ok(())
}

fn validate_derived_asset_bytes(
    asset_id: &str,
    label: &str,
    expected_sha256: Option<&str>,
    expected_byte_length: Option<u64>,
    bytes: &[u8],
) -> Result<()> {
    let (actual_sha256, actual_byte_length) = payload_integrity(bytes);
    match (expected_sha256, expected_byte_length) {
        (Some(expected_sha256), Some(expected_byte_length)) => {
            anyhow::ensure!(
                actual_sha256 == expected_sha256 && actual_byte_length == expected_byte_length,
                "asset integrity mismatch for {asset_id}: {label} expected sha256 {expected_sha256} and {expected_byte_length} bytes, found sha256 {actual_sha256} and {actual_byte_length} bytes"
            );
        }
        (None, None) => {}
        _ => bail!("asset integrity mismatch for {asset_id}: {label} metadata is incomplete"),
    }
    Ok(())
}

fn reconcile_derived_asset_integrity_metadata(
    asset_id: &str,
    label: &str,
    bytes: &[u8],
    expected_sha256: &mut Option<String>,
    expected_byte_length: &mut Option<u64>,
) -> Result<bool> {
    let (actual_sha256, actual_byte_length) = payload_integrity(bytes);
    match (expected_sha256.as_deref(), *expected_byte_length) {
        (Some(expected_sha256), Some(expected_byte_length)) => {
            anyhow::ensure!(
                actual_sha256 == expected_sha256 && actual_byte_length == expected_byte_length,
                "asset integrity mismatch for {asset_id}: {label} expected sha256 {expected_sha256} and {expected_byte_length} bytes, found sha256 {actual_sha256} and {actual_byte_length} bytes"
            );
            Ok(false)
        }
        (Some(expected_sha256), None) => {
            anyhow::ensure!(
                actual_sha256 == expected_sha256,
                "asset integrity mismatch for {asset_id}: {label} expected sha256 {expected_sha256}, found sha256 {actual_sha256}"
            );
            *expected_byte_length = Some(actual_byte_length);
            Ok(true)
        }
        (None, Some(expected_byte_length)) => {
            anyhow::ensure!(
                actual_byte_length == expected_byte_length,
                "asset integrity mismatch for {asset_id}: {label} expected {expected_byte_length} bytes, found {actual_byte_length} bytes"
            );
            *expected_sha256 = Some(actual_sha256);
            Ok(true)
        }
        (None, None) => {
            *expected_sha256 = Some(actual_sha256);
            *expected_byte_length = Some(actual_byte_length);
            Ok(true)
        }
    }
}

fn is_owned_derived_asset_uri(asset_id: &str, uri: &str, expected_kind: &str) -> bool {
    let Some(file_name) = owned_asset_uri_file_name(uri, expected_kind) else {
        return false;
    };
    match expected_kind {
        "text" => file_name == format!("{asset_id}.txt"),
        "preview" => file_name
            .strip_prefix(asset_id)
            .is_some_and(|suffix| suffix.starts_with('.') && suffix.len() > 1),
        _ => false,
    }
}

fn cleanup_asset_atomic_temp_files(root: &Path) -> Result<()> {
    for subdir in ["meta", "raw", "text", "preview", "tombstones"] {
        cleanup_asset_atomic_temp_files_in_dir(&root.join(subdir))?;
    }
    Ok(())
}

fn cleanup_asset_atomic_temp_files_in_dir(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("failed to read asset directory {}", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read asset directory {}", dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat asset file {}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !is_asset_atomic_temp_file_name(file_name) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove asset temp file {}",
                        entry.path().display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn is_asset_atomic_temp_file_name(file_name: &str) -> bool {
    let Some(candidate) = file_name.strip_prefix('.') else {
        return false;
    };
    let Some((target_name, temp_suffix)) = candidate.rsplit_once(".tmp-") else {
        return false;
    };
    if !is_asset_owned_file_name(target_name) {
        return false;
    }
    let Some((pid, nonce)) = temp_suffix.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !nonce.is_empty()
        && pid.chars().all(|value| value.is_ascii_digit())
        && nonce.chars().all(|value| value.is_ascii_digit())
}

fn is_asset_owned_file_name(file_name: &str) -> bool {
    let Some(suffix_part) = file_name.strip_prefix("asset-") else {
        return false;
    };
    let digit_len = suffix_part
        .chars()
        .take_while(|value| value.is_ascii_digit())
        .map(char::len_utf8)
        .sum::<usize>();
    if digit_len == 0 {
        return false;
    }
    let remainder = &suffix_part[digit_len..];
    remainder.is_empty()
        || (remainder.starts_with('.')
            && remainder[1..]
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || value == '.'))
}

fn asset_suffix_from_id(asset_id: &str) -> Option<u64> {
    asset_id
        .strip_prefix("asset-")
        .and_then(|value| value.parse::<u64>().ok())
}

fn asset_suffix_from_metadata_path(path: &Path) -> Option<u64> {
    let file_name = path.file_name()?.to_str()?;
    let suffix_part = file_name.strip_prefix("asset-")?;
    let digit_len = suffix_part
        .chars()
        .take_while(|value| value.is_ascii_digit())
        .map(char::len_utf8)
        .sum::<usize>();
    if digit_len == 0 {
        return None;
    }
    let remainder = &suffix_part[digit_len..];
    if !remainder.is_empty() && !remainder.starts_with('.') {
        return None;
    }
    suffix_part[..digit_len].parse::<u64>().ok()
}

fn asset_id_from_metadata_path(path: &Path) -> Option<String> {
    asset_suffix_from_metadata_path(path).map(|suffix| format!("asset-{suffix}"))
}

fn asset_id_from_exact_json_path(path: &Path) -> Option<(String, u64)> {
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return None;
    }
    let asset_id = path.file_stem()?.to_str()?;
    let suffix = asset_suffix_from_id(asset_id)?;
    Some((asset_id.to_string(), suffix))
}

fn load_tombstoned_asset_ids(
    root: &Path,
    repair_report: &mut AssetStartupRepairReport,
) -> Result<(BTreeSet<String>, u64)> {
    let mut tombstoned_asset_ids = BTreeSet::new();
    let mut max_suffix = 0u64;
    for entry in fs::read_dir(root.join("tombstones"))? {
        let entry = entry?;
        let path = entry.path();
        let Some((asset_id, suffix)) = asset_id_from_exact_json_path(&path) else {
            continue;
        };
        let Some(record) =
            read_json_or_quarantine::<AssetTombstoneRecord>(&path, "asset tombstone")?
        else {
            repair_report.invalid_tombstone_count += 1;
            repair_report.record(
                Some(&asset_id),
                "tombstone",
                "ignore",
                "tombstone_unreadable",
                None,
            );
            continue;
        };
        if record.asset_id != asset_id {
            repair_report.invalid_tombstone_count += 1;
            repair_report.record(
                Some(&asset_id),
                "tombstone",
                "ignore",
                "asset_id_mismatch",
                None,
            );
            continue;
        }
        max_suffix = max_suffix.max(suffix);
        tombstoned_asset_ids.insert(asset_id);
    }
    Ok((tombstoned_asset_ids, max_suffix))
}

fn complete_tombstoned_asset_deletions(
    root: &Path,
    asset_ids: &BTreeSet<String>,
    repair_report: &mut AssetStartupRepairReport,
) -> Result<()> {
    if asset_ids.is_empty() {
        return Ok(());
    }
    let mut removed_by_asset_id = BTreeMap::<String, usize>::new();
    for asset_id in asset_ids {
        let meta_path = root.join("meta").join(format!("{asset_id}.json"));
        if let Ok(bytes) = fs::read(&meta_path)
            && let Ok(record) = serde_json::from_slice::<StoredAssetRecord>(&bytes)
            && record.id == *asset_id
        {
            let removed = remove_asset_payload_files_for_record(root, &record)?;
            if removed > 0 {
                *removed_by_asset_id.entry(asset_id.clone()).or_default() += removed;
            }
        }
    }
    for (asset_id, removed) in remove_owned_payload_files_by_asset_ids(root, asset_ids)? {
        *removed_by_asset_id.entry(asset_id).or_default() += removed;
    }
    for asset_id in asset_ids {
        let removed =
            remove_asset_file_if_exists_count(root.join("meta").join(format!("{asset_id}.json")))?;
        if removed > 0 {
            *removed_by_asset_id.entry(asset_id.clone()).or_default() += removed;
        }
    }
    for (asset_id, removed_count) in removed_by_asset_id {
        if removed_count == 0 {
            continue;
        }
        repair_report.repaired_count += 1;
        repair_report.completed_tombstone_delete_count += 1;
        repair_report.record(
            Some(&asset_id),
            "tombstone",
            "complete_delete",
            "partial_delete_crash_window",
            None,
        );
    }
    Ok(())
}

fn remove_asset_payload_files_for_record(root: &Path, record: &StoredAssetRecord) -> Result<usize> {
    let mut removed = 0usize;
    if is_owned_raw_asset_uri(&record.id, &record.uri) {
        removed += remove_asset_file_if_exists_count(asset_path_from_uri(root, &record.uri)?)?;
    }
    if let Some(text_uri) = record.text_uri.as_deref()
        && is_owned_derived_asset_uri(&record.id, text_uri, "text")
    {
        removed += remove_asset_file_if_exists_count(asset_path_from_uri(root, text_uri)?)?;
    }
    if let Some(preview_uri) = record.preview_image_uri.as_deref()
        && is_owned_derived_asset_uri(&record.id, preview_uri, "preview")
    {
        removed += remove_asset_file_if_exists_count(asset_path_from_uri(root, preview_uri)?)?;
    }
    Ok(removed)
}

fn is_owned_raw_asset_uri(asset_id: &str, uri: &str) -> bool {
    let Some(file_name) = owned_asset_uri_file_name(uri, "raw") else {
        return false;
    };
    is_asset_payload_file_for_id(asset_id, file_name)
}

fn is_verified_attached_text_asset_uri(
    root: &Path,
    parent_asset_id: &str,
    uri: &str,
    bytes: &[u8],
) -> bool {
    let Some(file_name) = owned_asset_uri_file_name(uri, "raw") else {
        return false;
    };
    let Some(asset_id) = asset_id_from_owned_payload_file_name(file_name) else {
        return false;
    };
    if asset_id == parent_asset_id {
        return false;
    }
    let meta_path = root.join("meta").join(format!("{asset_id}.json"));
    let Ok(meta_bytes) = fs::read(meta_path) else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<StoredAssetRecord>(&meta_bytes) else {
        return false;
    };
    record.id == asset_id
        && record.media_type == "text/plain"
        && record.uri == uri
        && validate_raw_asset_bytes(&record, bytes).is_ok()
}

fn owned_asset_uri_file_name<'a>(uri: &'a str, expected_kind: &str) -> Option<&'a str> {
    let Some((kind, relative_path)) = parse_asset_storage_uri(uri) else {
        return None;
    };
    if kind != expected_kind {
        return None;
    }
    let path = Path::new(relative_path);
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return None;
    };
    if path != Path::new(file_name) {
        return None;
    }
    Some(file_name)
}

fn remove_owned_payload_files_by_asset_ids(
    root: &Path,
    asset_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, usize>> {
    let mut removed_by_asset_id = BTreeMap::<String, usize>::new();
    for kind in ["raw", "text", "preview"] {
        let dir = root.join(kind);
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read asset {kind} directory"))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let file_asset_id = asset_id_from_owned_payload_file_name(file_name);
            if is_asset_atomic_temp_file_name(file_name)
                || !file_asset_id.is_some_and(|asset_id| asset_ids.contains(asset_id))
            {
                continue;
            }
            let Some(file_asset_id) = asset_id_from_owned_payload_file_name(file_name) else {
                continue;
            };
            if remove_asset_file_if_exists_count(entry.path())? > 0 {
                *removed_by_asset_id
                    .entry(file_asset_id.to_string())
                    .or_default() += 1;
            }
        }
    }
    Ok(removed_by_asset_id)
}

fn is_asset_payload_file_for_id(asset_id: &str, file_name: &str) -> bool {
    if !is_asset_owned_file_name(file_name) {
        return false;
    }
    file_name == asset_id
        || file_name
            .strip_prefix(asset_id)
            .is_some_and(|suffix| suffix.starts_with('.') && suffix.len() > 1)
}

fn asset_id_from_owned_payload_file_name(file_name: &str) -> Option<&str> {
    if !is_asset_owned_file_name(file_name) {
        return None;
    }
    let suffix_part = file_name.strip_prefix("asset-")?;
    let digit_len = suffix_part
        .chars()
        .take_while(|value| value.is_ascii_digit())
        .map(char::len_utf8)
        .sum::<usize>();
    Some(&file_name[..("asset-".len() + digit_len)])
}

fn validate_loaded_asset_record(
    root: &Path,
    meta_path: &Path,
    mut record: StoredAssetRecord,
    repair_report: &mut AssetStartupRepairReport,
) -> Result<Option<StoredAssetRecord>> {
    let Some((meta_asset_id, _)) = asset_id_from_exact_json_path(meta_path) else {
        return Ok(None);
    };
    if record.id != meta_asset_id {
        repair_report.skipped_asset_count += 1;
        repair_report.record(
            Some(&meta_asset_id),
            "metadata",
            "skip_asset",
            "metadata_asset_id_mismatch",
            None,
        );
        return Ok(None);
    }
    if !is_owned_raw_asset_uri(&record.id, &record.uri) {
        repair_report.skipped_asset_count += 1;
        repair_report.record(
            Some(&record.id),
            "raw",
            "skip_asset",
            "raw_uri_not_owned_by_asset",
            Some(&record.uri),
        );
        return Ok(None);
    }
    let raw_path = asset_path_from_uri(root, &record.uri)?;
    let bytes = match fs::read(&raw_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            repair_report.skipped_asset_count += 1;
            repair_report.skipped_raw_missing_count += 1;
            repair_report.record(
                Some(&record.id),
                "raw",
                "skip_asset",
                "raw_payload_missing",
                Some(&record.uri),
            );
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read raw asset payload {}", raw_path.display())
            });
        }
    };
    if validate_raw_asset_bytes(&record, &bytes).is_err() {
        repair_report.skipped_asset_count += 1;
        repair_report.skipped_raw_integrity_mismatch_count += 1;
        repair_report.record(
            Some(&record.id),
            "raw",
            "skip_asset",
            "raw_integrity_mismatch",
            Some(&record.uri),
        );
        return Ok(None);
    }
    let expected_derived = match derive_asset_artifacts(&record.media_type, &bytes) {
        Ok(derived) => derived,
        Err(error) => {
            repair_report.skipped_asset_count += 1;
            let reason = format!("derived_artifacts_invalid: {error:#}");
            repair_report.record(
                Some(&record.id),
                "derived",
                "skip_asset",
                &reason,
                Some(&record.uri),
            );
            return Ok(None);
        }
    };

    let mut updated = false;
    let mut text_restore_reason = None::<&'static str>;
    if let Some(text_uri) = record.text_uri.clone() {
        let text_path = match asset_path_from_uri(root, &text_uri) {
            Ok(path) => path,
            Err(_) => {
                text_restore_reason = Some("derived_text_uri_invalid");
                record.text_uri = None;
                record.text_sha256 = None;
                record.text_byte_length = None;
                updated = true;
                PathBuf::new()
            }
        };
        if record.text_uri.is_some() {
            let text_bytes = fs::read(&text_path);
            if matches!(
                text_bytes.as_ref().map(|_| ()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ) {
                text_restore_reason = Some("derived_text_missing");
                record.text_uri = None;
                record.text_sha256 = None;
                record.text_byte_length = None;
                updated = true;
            } else {
                let text_bytes = text_bytes.with_context(|| {
                    format!("failed to read derived asset text {}", text_path.display())
                })?;
                let text = std::str::from_utf8(&text_bytes);
                let is_owned_text = is_owned_derived_asset_uri(&record.id, &text_uri, "text");
                let is_attached_text =
                    is_verified_attached_text_asset_uri(root, &record.id, &text_uri, &text_bytes);
                if text.is_err() {
                    text_restore_reason = Some("derived_text_invalid_utf8");
                    record.text_uri = None;
                    record.text_sha256 = None;
                    record.text_byte_length = None;
                    updated = true;
                } else if !is_owned_text && !is_attached_text {
                    text_restore_reason = Some("derived_text_uri_not_owned");
                    record.text_uri = None;
                    record.text_sha256 = None;
                    record.text_byte_length = None;
                    updated = true;
                } else if is_owned_text && expected_derived.text.as_deref() != text.ok() {
                    text_restore_reason = Some("derived_text_stale");
                    record.text_uri = None;
                    record.text_sha256 = None;
                    record.text_byte_length = None;
                    updated = true;
                } else {
                    match reconcile_derived_asset_integrity_metadata(
                        &record.id,
                        "derived text",
                        &text_bytes,
                        &mut record.text_sha256,
                        &mut record.text_byte_length,
                    ) {
                        Ok(changed) => {
                            if changed {
                                repair_report.repaired_count += 1;
                                repair_report.integrity_backfilled_count += 1;
                                repair_report.record(
                                    Some(&record.id),
                                    "derived_text",
                                    "backfill_integrity",
                                    "legacy_missing_integrity_metadata",
                                    Some(&text_uri),
                                );
                            }
                            updated |= changed;
                        }
                        Err(_) => {
                            text_restore_reason = Some("derived_text_integrity_mismatch");
                            record.text_uri = None;
                            record.text_sha256 = None;
                            record.text_byte_length = None;
                            updated = true;
                        }
                    }
                }
            }
        }
    }
    if record.text_uri.is_none()
        && (record.text_sha256.is_some() || record.text_byte_length.is_some())
    {
        text_restore_reason.get_or_insert("derived_text_reference_missing");
        record.text_sha256 = None;
        record.text_byte_length = None;
        updated = true;
    }
    if record.text_uri.is_none()
        && let Some(text) = expected_derived.text.as_deref()
    {
        restore_derived_asset_text(root, &mut record, text)?;
        repair_report.repaired_count += 1;
        repair_report.restored_derived_text_count += 1;
        repair_report.record(
            Some(&record.id),
            "derived_text",
            "restore",
            text_restore_reason.unwrap_or("derived_text_reference_missing"),
            record.text_uri.as_deref(),
        );
        updated = true;
    }
    let mut preview_restore_reason = None::<&'static str>;
    if let Some(preview_uri) = record.preview_image_uri.clone() {
        let preview_path = match asset_path_from_uri(root, &preview_uri) {
            Ok(path) => path,
            Err(_) => {
                preview_restore_reason = Some("derived_preview_uri_invalid");
                record.preview_image_uri = None;
                record.preview_image_media_type = None;
                record.preview_image_sha256 = None;
                record.preview_image_byte_length = None;
                updated = true;
                PathBuf::new()
            }
        };
        if record.preview_image_uri.is_some() {
            let preview_bytes = fs::read(&preview_path);
            if matches!(
                preview_bytes.as_ref().map(|_| ()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ) {
                preview_restore_reason = Some("derived_preview_missing");
                record.preview_image_uri = None;
                record.preview_image_media_type = None;
                record.preview_image_sha256 = None;
                record.preview_image_byte_length = None;
                updated = true;
            } else if !matches!(
                record.preview_image_media_type.as_deref(),
                Some("image/png" | "image/jpeg")
            ) || !is_owned_derived_asset_uri(&record.id, &preview_uri, "preview")
            {
                preview_restore_reason = Some("derived_preview_metadata_invalid");
                record.preview_image_uri = None;
                record.preview_image_media_type = None;
                record.preview_image_sha256 = None;
                record.preview_image_byte_length = None;
                updated = true;
            } else {
                let preview_bytes = preview_bytes.with_context(|| {
                    format!(
                        "failed to read derived asset preview {}",
                        preview_path.display()
                    )
                })?;
                let media_type = record.preview_image_media_type.clone().unwrap_or_default();
                if validate_media_type(&media_type, &preview_bytes).is_err() {
                    preview_restore_reason = Some("derived_preview_invalid_media");
                    record.preview_image_uri = None;
                    record.preview_image_media_type = None;
                    record.preview_image_sha256 = None;
                    record.preview_image_byte_length = None;
                    updated = true;
                } else if expected_derived.preview.as_ref().is_none_or(
                    |(expected_media_type, expected_bytes)| {
                        expected_media_type != &media_type || expected_bytes != &preview_bytes
                    },
                ) {
                    preview_restore_reason = Some("derived_preview_stale");
                    record.preview_image_uri = None;
                    record.preview_image_media_type = None;
                    record.preview_image_sha256 = None;
                    record.preview_image_byte_length = None;
                    updated = true;
                } else {
                    match reconcile_derived_asset_integrity_metadata(
                        &record.id,
                        "derived preview",
                        &preview_bytes,
                        &mut record.preview_image_sha256,
                        &mut record.preview_image_byte_length,
                    ) {
                        Ok(changed) => {
                            if changed {
                                repair_report.repaired_count += 1;
                                repair_report.integrity_backfilled_count += 1;
                                repair_report.record(
                                    Some(&record.id),
                                    "derived_preview",
                                    "backfill_integrity",
                                    "legacy_missing_integrity_metadata",
                                    Some(&preview_uri),
                                );
                            }
                            updated |= changed;
                        }
                        Err(_) => {
                            preview_restore_reason = Some("derived_preview_integrity_mismatch");
                            record.preview_image_uri = None;
                            record.preview_image_media_type = None;
                            record.preview_image_sha256 = None;
                            record.preview_image_byte_length = None;
                            updated = true;
                        }
                    }
                }
            }
        }
    }
    if record.preview_image_uri.is_none()
        && let Some((preview_media_type, preview_bytes)) = expected_derived.preview.as_ref()
    {
        restore_derived_asset_preview(root, &mut record, preview_media_type, preview_bytes)?;
        repair_report.repaired_count += 1;
        repair_report.restored_derived_preview_count += 1;
        repair_report.record(
            Some(&record.id),
            "derived_preview",
            "restore",
            preview_restore_reason.unwrap_or("derived_preview_reference_missing"),
            record.preview_image_uri.as_deref(),
        );
        updated = true;
    }
    if record.preview_image_uri.is_none() && record.preview_image_media_type.is_some() {
        preview_restore_reason.get_or_insert("derived_preview_reference_missing");
        record.preview_image_media_type = None;
        updated = true;
    }
    if record.preview_image_uri.is_none()
        && (record.preview_image_sha256.is_some() || record.preview_image_byte_length.is_some())
    {
        preview_restore_reason.get_or_insert("derived_preview_reference_missing");
        record.preview_image_sha256 = None;
        record.preview_image_byte_length = None;
        updated = true;
    }
    if updated {
        write_asset_metadata_atomic(meta_path, &record)?;
    }
    Ok(Some(record))
}

fn restore_derived_asset_text(
    root: &Path,
    record: &mut StoredAssetRecord,
    text: &str,
) -> Result<()> {
    let text_uri = asset_storage_uri("text", &format!("{}.txt", record.id));
    let text_path = asset_path_from_uri(root, &text_uri)?;
    write_file_atomic(&text_path, text.as_bytes()).with_context(|| {
        format!(
            "failed to restore derived asset text {}",
            text_path.display()
        )
    })?;
    let (text_sha256, text_byte_length) = payload_integrity(text.as_bytes());
    record.text_uri = Some(text_uri);
    record.text_sha256 = Some(text_sha256);
    record.text_byte_length = Some(text_byte_length);
    Ok(())
}

fn restore_derived_asset_preview(
    root: &Path,
    record: &mut StoredAssetRecord,
    media_type: &str,
    bytes: &[u8],
) -> Result<()> {
    let preview_extension = preferred_extension(media_type);
    let preview_uri = asset_storage_uri("preview", &format!("{}.{}", record.id, preview_extension));
    let preview_path = asset_path_from_uri(root, &preview_uri)?;
    write_file_atomic(&preview_path, bytes).with_context(|| {
        format!(
            "failed to restore derived asset preview {}",
            preview_path.display()
        )
    })?;
    let (preview_sha256, preview_byte_length) = payload_integrity(bytes);
    record.preview_image_uri = Some(preview_uri);
    record.preview_image_media_type = Some(media_type.to_string());
    record.preview_image_sha256 = Some(preview_sha256);
    record.preview_image_byte_length = Some(preview_byte_length);
    Ok(())
}

fn write_asset_metadata_atomic(path: &Path, record: &StoredAssetRecord) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record)?;
    write_file_atomic(path, &bytes)
        .with_context(|| format!("failed to write asset metadata {}", path.display()))
}

fn write_asset_tombstone_atomic(path: &Path, record: &AssetTombstoneRecord) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record)?;
    write_file_atomic(path, &bytes)
        .with_context(|| format!("failed to write asset tombstone {}", path.display()))
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    kheish_session::atomic_write(path, bytes)
}

fn asset_deletion_file_record(
    kind: &str,
    uri: Option<String>,
    path: PathBuf,
) -> Result<AssetDeletionFileRecord> {
    match fs::metadata(&path) {
        Ok(metadata) => Ok(AssetDeletionFileRecord {
            kind: kind.to_string(),
            uri,
            byte_length: metadata.len(),
            exists: true,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AssetDeletionFileRecord {
            kind: kind.to_string(),
            uri,
            byte_length: 0,
            exists: false,
        }),
        Err(error) => {
            Err(error).with_context(|| format!("failed to stat asset file {}", path.display()))
        }
    }
}

fn remove_asset_file_if_exists(path: PathBuf) -> Result<()> {
    remove_asset_file_if_exists_count(path).map(|_| ())
}

fn remove_asset_file_if_exists_count(path: PathBuf) -> Result<usize> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove asset file {}", path.display()))
        }
    }
}

fn asset_path_from_uri(root: &Path, uri: &str) -> Result<PathBuf> {
    let (kind, relative_path) =
        parse_asset_storage_uri(uri).ok_or_else(|| anyhow!("invalid asset storage uri '{uri}'"))?;
    let base = match kind {
        "raw" => root.join("raw"),
        "text" => root.join("text"),
        "preview" => root.join("preview"),
        other => bail!("unsupported asset storage kind '{other}'"),
    };
    validate_asset_relative_path(relative_path)?;
    Ok(base.join(relative_path))
}

fn validate_asset_relative_path(relative_path: &str) -> Result<()> {
    let path = Path::new(relative_path);
    anyhow::ensure!(
        !path.is_absolute(),
        "asset storage URI path must be relative"
    );
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                anyhow::ensure!(
                    !part.is_empty() && part != "." && part != "..",
                    "asset storage URI path contains an invalid component"
                );
            }
            _ => bail!("asset storage URI path contains an invalid component"),
        }
    }
    Ok(())
}

fn preferred_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "audio/wav" => "wav",
        "audio/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/mpga" => "mpga",
        "audio/opus" => "opus",
        "audio/aac" => "aac",
        "audio/flac" => "flac",
        "audio/mp4" => "mp4",
        "audio/m4a" => "m4a",
        "audio/pcm" | "audio/l16" | "audio/l24" => "pcm",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "application/dxf" => "dxf",
        "application/json" => "json",
        _ => "bin",
    }
}

fn normalize_media_type(
    file_name: &str,
    declared_media_type: Option<&str>,
    bytes: &[u8],
) -> Result<String> {
    let normalized_declared = declared_media_type
        .map(|value| {
            value
                .split_once(';')
                .map(|(media_type, _)| media_type)
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|value| !value.is_empty())
        .map(|value| match value.as_str() {
            "image/jpg" => "image/jpeg".to_string(),
            "application/csv" | "text/x-csv" | "application/vnd.ms-excel" => "text/csv".to_string(),
            "application/x-dxf" | "image/vnd.dxf" | "image/x-dxf" | "text/x-dxf" => {
                "application/dxf".to_string()
            }
            "audio/x-wav" => "audio/wav".to_string(),
            "audio/mp3" | "audio/x-mp3" => "audio/mpeg".to_string(),
            "audio/x-mpga" => "audio/mpga".to_string(),
            "audio/x-opus" => "audio/opus".to_string(),
            "audio/x-aac" => "audio/aac".to_string(),
            "audio/x-flac" => "audio/flac".to_string(),
            "audio/x-pcm" => "audio/pcm".to_string(),
            "audio/x-m4a" => "audio/m4a".to_string(),
            _ => value,
        });
    let detected = detect_media_type_from_content(file_name, bytes);
    let media_type = normalized_declared
        .clone()
        .or(detected.clone())
        .ok_or_else(|| anyhow!("unsupported attachment type for '{file_name}'"))?;
    if !is_supported_media_type(&media_type) {
        bail!("unsupported attachment media type '{media_type}'");
    }
    if let Some(detected) = detected.as_ref() {
        match media_type.as_str() {
            "image/png" | "image/jpeg" | "application/pdf" if detected != &media_type => {
                bail!(
                    "declared media type '{media_type}' does not match detected type '{detected}'"
                );
            }
            _ => {}
        }
    }
    validate_media_type(&media_type, bytes)?;
    Ok(media_type)
}

fn prepare_stored_payload(media_type: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    match media_type {
        "image/png" | "image/jpeg" => normalize_image_payload(media_type, bytes),
        _ => Ok(bytes.to_vec()),
    }
}

fn detect_media_type_from_content(file_name: &str, bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png".to_string());
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg".to_string());
    }
    if is_probable_wav(bytes) {
        return Some("audio/wav".to_string());
    }
    if is_probable_webm(bytes) {
        return Some("audio/webm".to_string());
    }
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf".to_string());
    }

    let extension = Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())?;
    match extension.as_str() {
        "txt" => Some("text/plain".to_string()),
        "csv" => Some("text/csv".to_string()),
        "md" | "markdown" => Some("text/markdown".to_string()),
        "json" => Some("application/json".to_string()),
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "wav" => Some("audio/wav".to_string()),
        "webm" => Some("audio/webm".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        "mpga" => Some("audio/mpga".to_string()),
        "opus" => Some("audio/opus".to_string()),
        "aac" => Some("audio/aac".to_string()),
        "flac" => Some("audio/flac".to_string()),
        "mp4" => Some("audio/mp4".to_string()),
        "m4a" => Some("audio/m4a".to_string()),
        "pcm" => Some("audio/l16".to_string()),
        "pdf" => Some("application/pdf".to_string()),
        "dxf" => Some("application/dxf".to_string()),
        _ => None,
    }
}

fn is_supported_media_type(media_type: &str) -> bool {
    is_supported_audio_media_type(media_type)
        || matches!(
            media_type,
            "image/png"
                | "image/jpeg"
                | "application/pdf"
                | "text/plain"
                | "text/csv"
                | "text/markdown"
                | "application/dxf"
                | "application/json"
        )
}

fn is_supported_audio_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "audio/wav"
            | "audio/webm"
            | "audio/mpeg"
            | "audio/mpga"
            | "audio/opus"
            | "audio/aac"
            | "audio/flac"
            | "audio/mp4"
            | "audio/m4a"
            | "audio/pcm"
            | "audio/l16"
            | "audio/l24"
    )
}

fn validate_media_type(media_type: &str, bytes: &[u8]) -> Result<()> {
    match media_type {
        "image/png" => {
            validate_image_payload_dimensions(
                media_type,
                bytes,
                MAX_SOURCE_IMAGE_EDGE_PX,
                MAX_SOURCE_IMAGE_PIXELS,
                "source",
            )?;
            decode_image_with_limits(
                media_type,
                bytes,
                MAX_SOURCE_IMAGE_EDGE_PX,
                MAX_IMAGE_DECODE_ALLOC_BYTES,
            )
            .context("attachment is not a decodable PNG image")?;
        }
        "image/jpeg" => {
            validate_image_payload_dimensions(
                media_type,
                bytes,
                MAX_SOURCE_IMAGE_EDGE_PX,
                MAX_SOURCE_IMAGE_PIXELS,
                "source",
            )?;
            decode_image_with_limits(
                media_type,
                bytes,
                MAX_SOURCE_IMAGE_EDGE_PX,
                MAX_IMAGE_DECODE_ALLOC_BYTES,
            )
            .context("attachment is not a decodable JPEG image")?;
        }
        audio if is_supported_audio_media_type(audio) => {
            validate_supported_audio_payload(audio, bytes)?
        }
        "application/pdf" => {
            if !bytes.starts_with(b"%PDF-") {
                bail!("attachment is not a valid PDF payload");
            }
        }
        "text/plain" | "text/csv" | "text/markdown" => {
            std::str::from_utf8(bytes).context("attachment is not valid UTF-8 text")?;
        }
        "application/dxf" => {
            validate_dxf_payload(bytes)?;
        }
        "application/json" => {
            let text = std::str::from_utf8(bytes).context("attachment is not valid UTF-8 JSON")?;
            let _: serde_json::Value =
                serde_json::from_str(text).context("attachment is not valid JSON")?;
        }
        _ => bail!("unsupported attachment media type '{media_type}'"),
    }
    Ok(())
}

pub(crate) fn validate_supported_audio_payload(media_type: &str, bytes: &[u8]) -> Result<()> {
    match media_type {
        "audio/wav" => validate_wav_payload(bytes),
        "audio/webm" => validate_webm_payload(bytes),
        "audio/mpeg" | "audio/mpga" => validate_mp3_payload(bytes),
        "audio/opus" => validate_ogg_opus_payload(bytes),
        "audio/aac" => validate_aac_payload(bytes),
        "audio/flac" => validate_flac_payload(bytes),
        "audio/mp4" | "audio/m4a" => validate_iso_bmff_audio_payload(bytes),
        "audio/pcm" => validate_pcm_payload(bytes, 2),
        "audio/l16" => validate_pcm_payload(bytes, 2),
        "audio/l24" => validate_pcm_payload(bytes, 3),
        other => bail!("unsupported audio media type '{other}'"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImageDimensions {
    width: u32,
    height: u32,
}

pub(crate) fn validate_image_payload_dimensions(
    media_type: &str,
    bytes: &[u8],
    max_edge_px: u32,
    max_pixels: u64,
    limit_name: &str,
) -> Result<()> {
    let (kind, dimensions) = match media_type {
        "image/png" => ("PNG", parse_png_dimensions(bytes)?),
        "image/jpeg" => ("JPEG", parse_jpeg_dimensions(bytes)?),
        other => bail!("unsupported image media type {other}"),
    };
    validate_image_dimensions(kind, dimensions, max_edge_px, max_pixels, limit_name)
}

pub(crate) fn decode_image_with_limits(
    media_type: &str,
    bytes: &[u8],
    max_edge_px: u32,
    max_alloc_bytes: u64,
) -> Result<DynamicImage> {
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        other => bail!("unsupported image media type {other}"),
    };
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_edge_px);
    limits.max_image_height = Some(max_edge_px);
    limits.max_alloc = Some(max_alloc_bytes);
    reader.limits(limits);
    reader
        .decode()
        .with_context(|| format!("failed to decode {media_type} image within daemon limits"))
}

fn validate_image_dimensions(
    kind: &str,
    dimensions: ImageDimensions,
    max_edge_px: u32,
    max_pixels: u64,
    limit_name: &str,
) -> Result<()> {
    anyhow::ensure!(
        dimensions.width > 0 && dimensions.height > 0,
        "attachment is not a valid {kind} payload: image dimensions must be positive"
    );
    let max_edge = dimensions.width.max(dimensions.height);
    if limit_name.is_empty() {
        anyhow::ensure!(
            max_edge <= max_edge_px,
            "attachment is not a valid {kind} payload: image dimensions exceed the {max_edge_px}px edge limit"
        );
    } else {
        anyhow::ensure!(
            max_edge <= max_edge_px,
            "attachment is not a valid {kind} payload: image dimensions exceed the {max_edge_px}px {limit_name} edge limit"
        );
    }
    let pixels = u64::from(dimensions.width) * u64::from(dimensions.height);
    if limit_name.is_empty() {
        anyhow::ensure!(
            pixels <= max_pixels,
            "attachment is not a valid {kind} payload: image dimensions exceed the {max_pixels} pixel limit"
        );
    } else {
        anyhow::ensure!(
            pixels <= max_pixels,
            "attachment is not a valid {kind} payload: image dimensions exceed the {max_pixels} {limit_name} pixel limit"
        );
    }
    Ok(())
}

fn parse_png_dimensions(bytes: &[u8]) -> Result<ImageDimensions> {
    anyhow::ensure!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "attachment is not a valid PNG payload"
    );
    anyhow::ensure!(bytes.len() >= 24, "PNG payload is truncated before IHDR");
    anyhow::ensure!(
        bytes.len() >= 33,
        "PNG payload is truncated before the IHDR checksum"
    );
    let ihdr_len = u32::from_be_bytes(bytes[8..12].try_into()?);
    anyhow::ensure!(ihdr_len == 13, "PNG IHDR chunk has an invalid length");
    anyhow::ensure!(&bytes[12..16] == b"IHDR", "PNG payload is missing IHDR");
    Ok(ImageDimensions {
        width: u32::from_be_bytes(bytes[16..20].try_into()?),
        height: u32::from_be_bytes(bytes[20..24].try_into()?),
    })
}

fn parse_jpeg_dimensions(bytes: &[u8]) -> Result<ImageDimensions> {
    anyhow::ensure!(
        bytes.len() >= 4 && bytes[0] == 0xff && bytes[1] == 0xd8,
        "JPEG payload is missing SOI"
    );
    let mut cursor = 2usize;
    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor] != 0xff {
            cursor += 1;
        }
        anyhow::ensure!(
            cursor < bytes.len(),
            "JPEG payload is missing a frame header"
        );
        while cursor < bytes.len() && bytes[cursor] == 0xff {
            cursor += 1;
        }
        anyhow::ensure!(cursor < bytes.len(), "JPEG marker is truncated");
        let marker = bytes[cursor];
        cursor += 1;

        if marker == 0xd9 {
            break;
        }
        if marker == 0xda {
            bail!("JPEG payload reached scan data before a frame header");
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }

        anyhow::ensure!(
            cursor + 2 <= bytes.len(),
            "JPEG segment length is truncated"
        );
        let segment_len = u16::from_be_bytes(bytes[cursor..cursor + 2].try_into()?) as usize;
        anyhow::ensure!(segment_len >= 2, "JPEG segment length is invalid");
        let payload_start = cursor + 2;
        let payload_end = payload_start
            .checked_add(segment_len - 2)
            .ok_or_else(|| anyhow!("JPEG segment offset overflow"))?;
        anyhow::ensure!(payload_end <= bytes.len(), "JPEG segment is truncated");

        if is_jpeg_start_of_frame_marker(marker) {
            anyhow::ensure!(segment_len >= 8, "JPEG frame header is truncated");
            return Ok(ImageDimensions {
                height: u16::from_be_bytes(bytes[payload_start + 1..payload_start + 3].try_into()?)
                    as u32,
                width: u16::from_be_bytes(bytes[payload_start + 3..payload_start + 5].try_into()?)
                    as u32,
            });
        }
        cursor = payload_end;
    }
    bail!("JPEG payload is missing a frame header")
}

fn is_jpeg_start_of_frame_marker(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn normalize_image_payload(media_type: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut image = decode_image_with_limits(
        media_type,
        bytes,
        MAX_SOURCE_IMAGE_EDGE_PX,
        MAX_IMAGE_DECODE_ALLOC_BYTES,
    )
    .with_context(|| format!("failed to decode {media_type} attachment during normalization"))?;
    let (width, height) = image.dimensions();
    let max_edge = width.max(height);
    if max_edge > MAX_IMAGE_EDGE_PX {
        let scale = MAX_IMAGE_EDGE_PX as f32 / max_edge as f32;
        let resized_width = ((width as f32 * scale).round() as u32).max(1);
        let resized_height = ((height as f32 * scale).round() as u32).max(1);
        image = image.resize(resized_width, resized_height, FilterType::Triangle);
    }

    let mut encoded = encode_image(&image, media_type)?;
    while encoded.len() > MAX_NORMALIZED_IMAGE_BYTES {
        let next_width = (image.width() / 2).max(1);
        let next_height = (image.height() / 2).max(1);
        if next_width == image.width() && next_height == image.height() {
            break;
        }
        image = image.resize(next_width, next_height, FilterType::Triangle);
        encoded = encode_image(&image, media_type)?;
    }
    if encoded.len() > MAX_NORMALIZED_IMAGE_BYTES {
        bail!(
            "normalized image exceeds the {} byte provider budget",
            MAX_NORMALIZED_IMAGE_BYTES
        );
    }
    Ok(encoded)
}

fn encode_image(image: &DynamicImage, media_type: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match media_type {
        "image/png" => image
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .context("failed to encode normalized PNG attachment")?,
        "image/jpeg" => {
            let rgb8 = image.to_rgb8();
            JpegEncoder::new_with_quality(&mut bytes, 85)
                .encode(
                    rgb8.as_raw(),
                    rgb8.width(),
                    rgb8.height(),
                    image::ColorType::Rgb8.into(),
                )
                .context("failed to encode normalized JPEG attachment")?;
        }
        other => bail!("unsupported image media type '{other}'"),
    }
    Ok(bytes)
}

struct DerivedAssetArtifacts {
    text: Option<String>,
    preview: Option<(String, Vec<u8>)>,
}

fn derive_asset_artifacts(media_type: &str, bytes: &[u8]) -> Result<DerivedAssetArtifacts> {
    let derived = match media_type {
        "text/plain" | "text/csv" | "text/markdown" => DerivedAssetArtifacts {
            text: Some(
                String::from_utf8(bytes.to_vec()).context("attachment is not valid UTF-8 text")?,
            ),
            preview: None,
        },
        "application/dxf" => derive_dxf_asset_artifacts(bytes)?,
        "application/json" => {
            let value: serde_json::Value =
                serde_json::from_slice(bytes).context("attachment is not valid JSON")?;
            DerivedAssetArtifacts {
                text: Some(
                    serde_json::to_string_pretty(&value)
                        .context("failed to pretty-print JSON attachment")?,
                ),
                preview: None,
            }
        }
        "application/pdf" => derive_pdf_asset_artifacts(bytes)?,
        "image/png" | "image/jpeg" => DerivedAssetArtifacts {
            text: None,
            preview: None,
        },
        "audio/wav" | "audio/webm" | "audio/mpeg" | "audio/mpga" | "audio/opus" | "audio/aac"
        | "audio/flac" | "audio/mp4" | "audio/m4a" | "audio/pcm" | "audio/l16" | "audio/l24" => {
            DerivedAssetArtifacts {
                text: None,
                preview: None,
            }
        }
        other => bail!("unsupported attachment media type '{other}'"),
    };

    Ok(DerivedAssetArtifacts {
        text: derived.text.map(|value| normalize_text(&value)),
        preview: derived.preview,
    })
}

fn normalize_text(value: &str) -> String {
    value.replace("\r\n", "\n").trim().to_string()
}

fn is_probable_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

fn validate_wav_payload(bytes: &[u8]) -> Result<()> {
    if !is_probable_wav(bytes) {
        bail!("attachment is not a valid WAV payload");
    }
    anyhow::ensure!(bytes.len() >= 44, "WAV payload is truncated");
    let declared_riff_size = u32::from_le_bytes(bytes[4..8].try_into().expect("riff size"));
    let riff_end = if declared_riff_size == u32::MAX {
        bytes.len()
    } else {
        let riff_size = declared_riff_size as usize;
        let riff_end = riff_size
            .checked_add(8)
            .ok_or_else(|| anyhow!("WAV declared length overflows addressable memory"))?;
        anyhow::ensure!(
            riff_end <= bytes.len(),
            "WAV payload declared length exceeds the provided bytes"
        );
        riff_end
    };

    let mut cursor = 12usize;
    let mut wav_format = None;
    let mut saw_non_empty_data = false;
    let mut data_chunk_sizes = Vec::new();
    while cursor + 8 <= riff_end {
        let chunk_id = &bytes[cursor..cursor + 4];
        let declared_chunk_size = u32::from_le_bytes(
            bytes[cursor + 4..cursor + 8]
                .try_into()
                .expect("chunk size"),
        );
        cursor += 8;
        let streaming_data_chunk = chunk_id == b"data" && declared_chunk_size == u32::MAX;
        let chunk_size = if streaming_data_chunk {
            riff_end
                .checked_sub(cursor)
                .ok_or_else(|| anyhow!("WAV streaming data chunk offset overflow"))?
        } else {
            declared_chunk_size as usize
        };
        anyhow::ensure!(
            cursor
                .checked_add(chunk_size)
                .is_some_and(|end| end <= riff_end),
            "WAV chunk exceeds the provided bytes"
        );
        match chunk_id {
            b"fmt " => {
                anyhow::ensure!(wav_format.is_none(), "WAV payload has multiple fmt chunks");
                wav_format = Some(parse_wav_format_chunk(&bytes[cursor..cursor + chunk_size])?);
            }
            b"data" if chunk_size > 0 => {
                saw_non_empty_data = true;
                data_chunk_sizes.push(chunk_size);
            }
            _ => {}
        }
        cursor += chunk_size;
        if !streaming_data_chunk && chunk_size % 2 == 1 {
            anyhow::ensure!(cursor < riff_end, "WAV chunk padding is truncated");
            cursor += 1;
        }
    }
    anyhow::ensure!(
        cursor == riff_end,
        "WAV payload ended inside a partial chunk header"
    );
    let wav_format = wav_format.ok_or_else(|| anyhow!("WAV payload is missing fmt chunk"))?;
    validate_wav_format(wav_format, &data_chunk_sizes)?;
    anyhow::ensure!(
        saw_non_empty_data,
        "WAV payload is missing non-empty audio data"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct WavFormatInfo {
    audio_format: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
}

fn parse_wav_format_chunk(bytes: &[u8]) -> Result<WavFormatInfo> {
    anyhow::ensure!(bytes.len() >= 16, "WAV fmt chunk is truncated");
    let declared_audio_format = u16::from_le_bytes(bytes[0..2].try_into()?);
    let audio_format = if declared_audio_format == 0xfffe {
        anyhow::ensure!(bytes.len() >= 40, "WAV extensible fmt chunk is truncated");
        let extension_size = u16::from_le_bytes(bytes[16..18].try_into()?) as usize;
        anyhow::ensure!(
            extension_size >= 22 && bytes.len() >= 18 + extension_size,
            "WAV extensible fmt chunk extension is truncated"
        );
        let valid_bits_per_sample = u16::from_le_bytes(bytes[18..20].try_into()?);
        let bits_per_sample = u16::from_le_bytes(bytes[14..16].try_into()?);
        anyhow::ensure!(
            valid_bits_per_sample > 0 && valid_bits_per_sample <= bits_per_sample,
            "WAV extensible valid bits per sample is invalid"
        );
        wav_extensible_subformat(&bytes[24..40])?
    } else {
        declared_audio_format
    };
    Ok(WavFormatInfo {
        audio_format,
        channels: u16::from_le_bytes(bytes[2..4].try_into()?),
        sample_rate: u32::from_le_bytes(bytes[4..8].try_into()?),
        byte_rate: u32::from_le_bytes(bytes[8..12].try_into()?),
        block_align: u16::from_le_bytes(bytes[12..14].try_into()?),
        bits_per_sample: u16::from_le_bytes(bytes[14..16].try_into()?),
    })
}

fn wav_extensible_subformat(guid: &[u8]) -> Result<u16> {
    const PCM_SUBFORMAT: [u8; 16] = [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ];
    const FLOAT_SUBFORMAT: [u8; 16] = [
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ];
    if guid == PCM_SUBFORMAT {
        Ok(1)
    } else if guid == FLOAT_SUBFORMAT {
        Ok(3)
    } else {
        bail!("WAV extensible subformat is not supported")
    }
}

fn validate_wav_format(format: WavFormatInfo, data_chunk_sizes: &[usize]) -> Result<()> {
    anyhow::ensure!(
        matches!(format.audio_format, 1 | 3 | 0xfffe),
        "WAV audio format {} is not supported",
        format.audio_format
    );
    anyhow::ensure!(
        (1..=MAX_AUDIO_CHANNELS).contains(&format.channels),
        "WAV channel count {} is outside the 1..={} supported range",
        format.channels,
        MAX_AUDIO_CHANNELS
    );
    anyhow::ensure!(
        (MIN_AUDIO_SAMPLE_RATE_HZ..=MAX_AUDIO_SAMPLE_RATE_HZ).contains(&format.sample_rate),
        "WAV sample rate {} is outside the {}..={} Hz supported range",
        format.sample_rate,
        MIN_AUDIO_SAMPLE_RATE_HZ,
        MAX_AUDIO_SAMPLE_RATE_HZ
    );
    let supported_bits = match format.audio_format {
        3 => matches!(format.bits_per_sample, 32 | 64),
        _ => matches!(format.bits_per_sample, 8 | 16 | 24 | 32),
    };
    anyhow::ensure!(
        supported_bits,
        "WAV bits per sample {} is not supported",
        format.bits_per_sample
    );
    anyhow::ensure!(
        format.bits_per_sample % 8 == 0,
        "WAV bits per sample must be byte-aligned"
    );
    let bytes_per_sample = format.bits_per_sample / 8;
    let expected_block_align = format
        .channels
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| anyhow!("WAV block alignment overflow"))?;
    anyhow::ensure!(
        format.block_align == expected_block_align,
        "WAV block_align {} does not match channels/bits_per_sample {}",
        format.block_align,
        expected_block_align
    );
    let expected_byte_rate = format
        .sample_rate
        .checked_mul(format.block_align as u32)
        .ok_or_else(|| anyhow!("WAV byte rate overflow"))?;
    anyhow::ensure!(
        format.byte_rate == expected_byte_rate,
        "WAV byte_rate {} does not match sample_rate*block_align {}",
        format.byte_rate,
        expected_byte_rate
    );
    for data_chunk_size in data_chunk_sizes {
        anyhow::ensure!(
            data_chunk_size % usize::from(format.block_align) == 0,
            "WAV data byte length is not aligned to block_align {}",
            format.block_align
        );
    }
    let total_data_bytes = data_chunk_sizes
        .iter()
        .try_fold(0usize, |total, size| total.checked_add(*size))
        .ok_or_else(|| anyhow!("WAV data byte length overflow"))?;
    let duration_ms = duration_millis_from_bytes(total_data_bytes, format.byte_rate)?;
    validate_audio_stream_limits("WAV", None, None, Some(duration_ms))?;
    Ok(())
}

fn validate_audio_stream_limits(
    kind: &str,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    duration_ms: Option<u64>,
) -> Result<()> {
    if let Some(sample_rate_hz) = sample_rate_hz {
        anyhow::ensure!(
            (MIN_AUDIO_SAMPLE_RATE_HZ..=MAX_AUDIO_SAMPLE_RATE_HZ).contains(&sample_rate_hz),
            "{kind} sample rate {} is outside the {}..={} Hz supported range",
            sample_rate_hz,
            MIN_AUDIO_SAMPLE_RATE_HZ,
            MAX_AUDIO_SAMPLE_RATE_HZ
        );
    }
    if let Some(channels) = channels {
        anyhow::ensure!(
            (1..=MAX_AUDIO_CHANNELS).contains(&channels),
            "{kind} channel count {} is outside the 1..={} supported range",
            channels,
            MAX_AUDIO_CHANNELS
        );
    }
    if let Some(duration_ms) = duration_ms {
        anyhow::ensure!(
            duration_ms <= MAX_AUDIO_DURATION_MS,
            "{kind} audio duration {}ms exceeds the {}ms daemon limit",
            duration_ms,
            MAX_AUDIO_DURATION_MS
        );
    }
    Ok(())
}

fn duration_millis_from_bytes(byte_count: usize, byte_rate: u32) -> Result<u64> {
    anyhow::ensure!(byte_rate > 0, "audio byte rate must be positive");
    let millis = (byte_count as u128)
        .checked_mul(1000)
        .ok_or_else(|| anyhow!("audio duration overflow"))?
        .div_ceil(byte_rate as u128);
    u64::try_from(millis).map_err(|_| anyhow!("audio duration exceeds addressable range"))
}

fn duration_millis_from_samples(sample_count: u64, sample_rate_hz: u32) -> Result<u64> {
    anyhow::ensure!(sample_rate_hz > 0, "audio sample rate must be positive");
    let millis = (sample_count as u128)
        .checked_mul(1000)
        .ok_or_else(|| anyhow!("audio duration overflow"))?
        .div_ceil(sample_rate_hz as u128);
    u64::try_from(millis).map_err(|_| anyhow!("audio duration exceeds addressable range"))
}

fn is_probable_webm(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) && bytes.windows(4).any(|window| window == b"webm")
}

fn validate_webm_payload(bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(
        is_probable_webm(bytes),
        "attachment is not a valid WebM payload"
    );
    anyhow::ensure!(bytes.len() >= 32, "WebM payload is truncated");
    let mut cursor = 0usize;
    let mut saw_ebml_header = false;
    let mut saw_segment = false;
    let mut saw_audio_track = false;
    let mut saw_media_data = false;
    while let Some(element) = read_ebml_element(bytes, cursor, bytes.len())? {
        match element.id {
            EBML_ID_EBML => {
                saw_ebml_header = true;
                anyhow::ensure!(
                    webm_ebml_header_has_doctype(
                        &bytes[element.payload_start..element.payload_end]
                    )?,
                    "WebM payload is missing EBML DocType `webm`"
                );
            }
            EBML_ID_SEGMENT => {
                saw_segment = true;
                let info = parse_webm_segment(&bytes[element.payload_start..element.payload_end])?;
                saw_audio_track |= info.saw_audio_track;
                saw_media_data |= info.saw_media_data;
            }
            EBML_ID_VOID | EBML_ID_CRC32 => {}
            _ => {}
        }
        cursor = element.payload_end;
    }
    anyhow::ensure!(saw_ebml_header, "WebM payload is missing EBML header");
    anyhow::ensure!(saw_segment, "WebM payload is missing Segment");
    anyhow::ensure!(
        saw_audio_track,
        "WebM payload does not contain a supported Opus/Vorbis audio track"
    );
    anyhow::ensure!(
        saw_media_data,
        "WebM payload does not contain non-empty media data"
    );
    Ok(())
}

const EBML_ID_EBML: u64 = 0x1a45dfa3;
const EBML_ID_SEGMENT: u64 = 0x18538067;
const EBML_ID_DOCTYPE: u64 = 0x4282;
const EBML_ID_TRACKS: u64 = 0x1654ae6b;
const EBML_ID_TRACK_ENTRY: u64 = 0xae;
const EBML_ID_TRACK_TYPE: u64 = 0x83;
const EBML_ID_CODEC_ID: u64 = 0x86;
const EBML_ID_AUDIO: u64 = 0xe1;
const EBML_ID_AUDIO_CHANNELS: u64 = 0x9f;
const EBML_ID_AUDIO_SAMPLING_FREQUENCY: u64 = 0xb5;
const EBML_ID_CLUSTER: u64 = 0x1f43b675;
const EBML_ID_SIMPLE_BLOCK: u64 = 0xa3;
const EBML_ID_BLOCK_GROUP: u64 = 0xa0;
const EBML_ID_BLOCK: u64 = 0xa1;
const EBML_ID_VOID: u64 = 0xec;
const EBML_ID_CRC32: u64 = 0xbf;

#[derive(Clone, Copy, Debug)]
struct EbmlElement {
    id: u64,
    payload_start: usize,
    payload_end: usize,
}

#[derive(Default)]
struct WebmSegmentInfo {
    saw_audio_track: bool,
    saw_media_data: bool,
}

#[derive(Default)]
struct WebmTrackEntry {
    track_type: Option<u64>,
    codec_id: Option<String>,
}

fn read_ebml_element(bytes: &[u8], cursor: usize, limit: usize) -> Result<Option<EbmlElement>> {
    if cursor >= limit {
        return Ok(None);
    }
    anyhow::ensure!(
        limit <= bytes.len(),
        "EBML parser limit exceeds payload length"
    );
    let (id, id_len) = read_ebml_id(bytes, cursor, limit)?;
    let size_cursor = cursor
        .checked_add(id_len)
        .ok_or_else(|| anyhow!("EBML element offset overflow"))?;
    let (size, size_len, unknown_size) = read_ebml_size(bytes, size_cursor, limit)?;
    let payload_start = size_cursor
        .checked_add(size_len)
        .ok_or_else(|| anyhow!("EBML element payload offset overflow"))?;
    let payload_end = if unknown_size {
        limit
    } else {
        payload_start
            .checked_add(size)
            .ok_or_else(|| anyhow!("EBML element size overflow"))?
    };
    anyhow::ensure!(
        payload_end <= limit,
        "EBML element declared length exceeds payload"
    );
    Ok(Some(EbmlElement {
        id,
        payload_start,
        payload_end,
    }))
}

fn read_ebml_id(bytes: &[u8], cursor: usize, limit: usize) -> Result<(u64, usize)> {
    anyhow::ensure!(cursor < limit, "EBML element id is truncated");
    let first = bytes[cursor];
    let len = ebml_vint_length(first).ok_or_else(|| anyhow!("invalid EBML element id"))?;
    anyhow::ensure!(len <= 4, "EBML element id is too long");
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| anyhow!("EBML element id offset overflow"))?;
    anyhow::ensure!(end <= limit, "EBML element id is truncated");
    let id = bytes[cursor..end]
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
    Ok((id, len))
}

fn read_ebml_size(bytes: &[u8], cursor: usize, limit: usize) -> Result<(usize, usize, bool)> {
    anyhow::ensure!(cursor < limit, "EBML element size is truncated");
    let first = bytes[cursor];
    let len = ebml_vint_length(first).ok_or_else(|| anyhow!("invalid EBML element size"))?;
    anyhow::ensure!(len <= 8, "EBML element size is too long");
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| anyhow!("EBML element size offset overflow"))?;
    anyhow::ensure!(end <= limit, "EBML element size is truncated");
    let marker_mask = 0x80u8 >> (len - 1);
    let mut value = u64::from(first & !marker_mask);
    for byte in &bytes[cursor + 1..end] {
        value = (value << 8) | u64::from(*byte);
    }
    let max_value = (1u64 << (7 * len)) - 1;
    let unknown_size = value == max_value;
    let size = usize::try_from(value).map_err(|_| anyhow!("EBML element size is too large"))?;
    Ok((size, len, unknown_size))
}

fn ebml_vint_length(first: u8) -> Option<usize> {
    (0..8)
        .find(|shift| first & (0x80u8 >> shift) != 0)
        .map(|shift| shift + 1)
}

fn webm_ebml_header_has_doctype(payload: &[u8]) -> Result<bool> {
    let mut cursor = 0usize;
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        if element.id == EBML_ID_DOCTYPE {
            let doc_type =
                std::str::from_utf8(&payload[element.payload_start..element.payload_end])
                    .context("WebM EBML DocType is not UTF-8")?;
            return Ok(doc_type == "webm");
        }
        cursor = element.payload_end;
    }
    Ok(false)
}

fn parse_webm_segment(payload: &[u8]) -> Result<WebmSegmentInfo> {
    let mut cursor = 0usize;
    let mut info = WebmSegmentInfo::default();
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        match element.id {
            EBML_ID_TRACKS => {
                info.saw_audio_track |= webm_tracks_have_supported_audio(
                    &payload[element.payload_start..element.payload_end],
                )?;
            }
            EBML_ID_CLUSTER => {
                info.saw_media_data |= webm_cluster_has_media_data(
                    &payload[element.payload_start..element.payload_end],
                )?;
            }
            _ => {}
        }
        cursor = element.payload_end;
    }
    Ok(info)
}

fn webm_tracks_have_supported_audio(payload: &[u8]) -> Result<bool> {
    let mut cursor = 0usize;
    let mut saw_audio_track = false;
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        if element.id == EBML_ID_TRACK_ENTRY {
            let track =
                parse_webm_track_entry(&payload[element.payload_start..element.payload_end])?;
            if track.track_type == Some(2) {
                saw_audio_track = true;
                let codec_id = track
                    .codec_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("WebM audio track is missing CodecID"))?;
                anyhow::ensure!(
                    matches!(codec_id, "A_OPUS" | "A_VORBIS"),
                    "WebM audio codec {codec_id} is not supported"
                );
            }
        }
        cursor = element.payload_end;
    }
    Ok(saw_audio_track)
}

fn parse_webm_track_entry(payload: &[u8]) -> Result<WebmTrackEntry> {
    let mut cursor = 0usize;
    let mut track = WebmTrackEntry::default();
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        let element_payload = &payload[element.payload_start..element.payload_end];
        match element.id {
            EBML_ID_TRACK_TYPE => track.track_type = Some(parse_ebml_uint(element_payload)?),
            EBML_ID_CODEC_ID => {
                track.codec_id = Some(
                    std::str::from_utf8(element_payload)
                        .context("WebM CodecID is not UTF-8")?
                        .to_string(),
                );
            }
            EBML_ID_AUDIO => validate_webm_audio_metadata(element_payload)?,
            _ => {}
        }
        cursor = element.payload_end;
    }
    Ok(track)
}

fn validate_webm_audio_metadata(payload: &[u8]) -> Result<()> {
    let mut cursor = 0usize;
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        let element_payload = &payload[element.payload_start..element.payload_end];
        match element.id {
            EBML_ID_AUDIO_CHANNELS => {
                let channels = parse_ebml_uint(element_payload)?;
                let channels = u16::try_from(channels)
                    .map_err(|_| anyhow!("WebM audio channel count is too large"))?;
                validate_audio_stream_limits("WebM", None, Some(channels), None)?;
            }
            EBML_ID_AUDIO_SAMPLING_FREQUENCY => {
                let sample_rate = parse_ebml_float(element_payload)?;
                anyhow::ensure!(
                    sample_rate.is_finite() && sample_rate > 0.0,
                    "WebM audio sampling frequency is invalid"
                );
                let sample_rate_hz = sample_rate.round() as u32;
                validate_audio_stream_limits("WebM", Some(sample_rate_hz), None, None)?;
            }
            _ => {}
        }
        cursor = element.payload_end;
    }
    Ok(())
}

fn parse_ebml_uint(payload: &[u8]) -> Result<u64> {
    anyhow::ensure!(
        (1..=8).contains(&payload.len()),
        "EBML unsigned integer has invalid length"
    );
    Ok(payload
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn parse_ebml_float(payload: &[u8]) -> Result<f64> {
    match payload.len() {
        4 => Ok(f32::from_be_bytes(payload.try_into()?) as f64),
        8 => Ok(f64::from_be_bytes(payload.try_into()?)),
        _ => bail!("EBML float has invalid length"),
    }
}

fn webm_cluster_has_media_data(payload: &[u8]) -> Result<bool> {
    let mut cursor = 0usize;
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        let element_payload = &payload[element.payload_start..element.payload_end];
        match element.id {
            EBML_ID_SIMPLE_BLOCK if webm_block_has_frame_payload(element_payload)? => {
                return Ok(true);
            }
            EBML_ID_BLOCK_GROUP if webm_block_group_has_media_data(element_payload)? => {
                return Ok(true);
            }
            _ => {}
        }
        cursor = element.payload_end;
    }
    Ok(false)
}

fn webm_block_group_has_media_data(payload: &[u8]) -> Result<bool> {
    let mut cursor = 0usize;
    while let Some(element) = read_ebml_element(payload, cursor, payload.len())? {
        if element.id == EBML_ID_BLOCK
            && webm_block_has_frame_payload(&payload[element.payload_start..element.payload_end])?
        {
            return Ok(true);
        }
        cursor = element.payload_end;
    }
    Ok(false)
}

fn webm_block_has_frame_payload(payload: &[u8]) -> Result<bool> {
    anyhow::ensure!(!payload.is_empty(), "WebM block is empty");
    let track_number_len = ebml_vint_length(payload[0])
        .ok_or_else(|| anyhow!("WebM block track number is invalid"))?;
    let frame_start = track_number_len
        .checked_add(3)
        .ok_or_else(|| anyhow!("WebM block header offset overflow"))?;
    anyhow::ensure!(
        payload.len() >= frame_start,
        "WebM block header is truncated"
    );
    Ok(payload.len() > frame_start)
}

fn validate_mp3_payload(bytes: &[u8]) -> Result<()> {
    if let Some(frame) = mp3_frame_info(bytes) {
        anyhow::ensure!(
            bytes.len() >= frame.frame_length,
            "MP3 audio frame is truncated: {} bytes provided, {} bytes required",
            bytes.len(),
            frame.frame_length
        );
        validate_audio_stream_limits(
            "MP3",
            Some(frame.sample_rate_hz),
            Some(frame.channels),
            Some(duration_millis_from_samples(
                frame.samples_per_frame,
                frame.sample_rate_hz,
            )?),
        )?;
        return Ok(());
    }
    if bytes.starts_with(b"ID3") {
        anyhow::ensure!(bytes.len() >= 10, "MP3 ID3 tag is truncated");
        let tag_size = parse_id3_syncsafe_size(&bytes[6..10])?;
        let footer_size = if bytes[5] & 0x10 != 0 { 10 } else { 0 };
        let frame_offset = 10usize
            .checked_add(tag_size)
            .and_then(|value| value.checked_add(footer_size))
            .ok_or_else(|| anyhow!("MP3 ID3 tag size overflow"))?;
        if let Some(frame_bytes) = bytes.get(frame_offset..) {
            let Some(frame) = mp3_frame_info(frame_bytes) else {
                bail!("attachment ID3 tag does not contain an MP3 audio frame");
            };
            anyhow::ensure!(
                frame_bytes.len() >= frame.frame_length,
                "MP3 audio frame after ID3 tag is truncated: {} bytes provided, {} bytes required",
                frame_bytes.len(),
                frame.frame_length
            );
            validate_audio_stream_limits(
                "MP3",
                Some(frame.sample_rate_hz),
                Some(frame.channels),
                Some(duration_millis_from_samples(
                    frame.samples_per_frame,
                    frame.sample_rate_hz,
                )?),
            )?;
            return Ok(());
        }
        bail!("attachment ID3 tag does not contain an MP3 audio frame");
    }
    bail!("attachment is not a valid MP3/MPEG payload");
}

#[derive(Clone, Copy, Debug)]
struct Mp3FrameInfo {
    frame_length: usize,
    sample_rate_hz: u32,
    channels: u16,
    samples_per_frame: u64,
}

fn parse_id3_syncsafe_size(bytes: &[u8]) -> Result<usize> {
    anyhow::ensure!(bytes.len() == 4, "ID3 syncsafe size must be four bytes");
    let mut size = 0usize;
    for byte in bytes {
        anyhow::ensure!(
            byte & 0x80 == 0,
            "MP3 ID3 tag uses an invalid syncsafe size"
        );
        size = (size << 7) | (*byte as usize);
    }
    Ok(size)
}

fn mp3_frame_info(bytes: &[u8]) -> Option<Mp3FrameInfo> {
    if bytes.len() < 4
        || bytes[0] != 0xff
        || (bytes[1] & 0xe0) != 0xe0
        || (bytes[1] & 0x18) == 0x08
        || (bytes[1] & 0x06) == 0
        || (bytes[2] & 0xf0) == 0xf0
        || (bytes[2] & 0x0c) == 0x0c
    {
        return None;
    }

    let version_id = (bytes[1] >> 3) & 0x03;
    let layer_id = (bytes[1] >> 1) & 0x03;
    let bitrate_index = (bytes[2] >> 4) as usize;
    if bitrate_index == 0 {
        return None;
    }
    let sample_rate_index = ((bytes[2] >> 2) & 0x03) as usize;
    let padding = ((bytes[2] >> 1) & 0x01) as usize;
    let sample_rate = mp3_sample_rate(version_id, sample_rate_index)?;
    let bitrate_kbps = mp3_bitrate_kbps(version_id, layer_id, bitrate_index)?;
    let channels = if (bytes[3] >> 6) == 0b11 { 1 } else { 2 };
    let samples_per_frame = mp3_samples_per_frame(version_id, layer_id)?;

    let frame_length = if layer_id == 0b11 {
        ((12 * bitrate_kbps * 1000 / sample_rate) + padding) * 4
    } else {
        let coefficient = if layer_id == 0b01 && version_id != 0b11 {
            72
        } else {
            144
        };
        coefficient * bitrate_kbps * 1000 / sample_rate + padding
    };
    (frame_length >= 4).then_some(Mp3FrameInfo {
        frame_length,
        sample_rate_hz: u32::try_from(sample_rate).ok()?,
        channels,
        samples_per_frame,
    })
}

fn mp3_sample_rate(version_id: u8, sample_rate_index: usize) -> Option<usize> {
    const MPEG1: [usize; 3] = [44_100, 48_000, 32_000];
    const MPEG2: [usize; 3] = [22_050, 24_000, 16_000];
    const MPEG25: [usize; 3] = [11_025, 12_000, 8_000];
    match version_id {
        0b11 => MPEG1.get(sample_rate_index).copied(),
        0b10 => MPEG2.get(sample_rate_index).copied(),
        0b00 => MPEG25.get(sample_rate_index).copied(),
        _ => None,
    }
}

fn mp3_bitrate_kbps(version_id: u8, layer_id: u8, bitrate_index: usize) -> Option<usize> {
    const MPEG1_LAYER1: [usize; 16] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
    ];
    const MPEG1_LAYER2: [usize; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ];
    const MPEG1_LAYER3: [usize; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const MPEG2_LAYER1: [usize; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
    ];
    const MPEG2_LAYER23: [usize; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    let table = match (version_id == 0b11, layer_id) {
        (true, 0b11) => &MPEG1_LAYER1,
        (true, 0b10) => &MPEG1_LAYER2,
        (true, 0b01) => &MPEG1_LAYER3,
        (false, 0b11) => &MPEG2_LAYER1,
        (false, 0b10 | 0b01) => &MPEG2_LAYER23,
        _ => return None,
    };
    table.get(bitrate_index).copied().filter(|value| *value > 0)
}

fn mp3_samples_per_frame(version_id: u8, layer_id: u8) -> Option<u64> {
    match layer_id {
        0b11 => Some(384),
        0b10 => Some(1152),
        0b01 if version_id == 0b11 => Some(1152),
        0b01 => Some(576),
        _ => None,
    }
}

fn validate_ogg_opus_payload(bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(bytes.len() >= 28, "Ogg Opus payload is truncated");
    anyhow::ensure!(
        bytes.starts_with(b"OggS"),
        "attachment is not an Ogg payload"
    );
    anyhow::ensure!(bytes[4] == 0, "Ogg payload uses an unsupported version");
    let page_segments = bytes[26] as usize;
    let segment_table_end = 27usize
        .checked_add(page_segments)
        .ok_or_else(|| anyhow!("Ogg segment table offset overflow"))?;
    anyhow::ensure!(
        segment_table_end <= bytes.len(),
        "Ogg Opus segment table is truncated"
    );
    let body_len = bytes[27..segment_table_end]
        .iter()
        .try_fold(0usize, |total, segment| {
            total.checked_add(*segment as usize)
        })
        .ok_or_else(|| anyhow!("Ogg Opus body size overflow"))?;
    let body_end = segment_table_end
        .checked_add(body_len)
        .ok_or_else(|| anyhow!("Ogg Opus body offset overflow"))?;
    anyhow::ensure!(body_end <= bytes.len(), "Ogg Opus page body is truncated");
    let body = &bytes[segment_table_end..body_end];
    anyhow::ensure!(
        body.starts_with(b"OpusHead"),
        "attachment is not a valid Ogg Opus payload"
    );
    anyhow::ensure!(body.len() >= 19, "Ogg Opus OpusHead packet is truncated");
    let channels = body[9] as u16;
    let input_sample_rate = u32::from_le_bytes(body[12..16].try_into()?);
    validate_audio_stream_limits(
        "Ogg Opus",
        (input_sample_rate > 0).then_some(input_sample_rate),
        Some(channels),
        None,
    )?;
    Ok(())
}

fn validate_aac_payload(bytes: &[u8]) -> Result<()> {
    if bytes.starts_with(b"ADIF") {
        return Ok(());
    }
    anyhow::ensure!(
        bytes.len() >= 7 && bytes[0] == 0xff && (bytes[1] & 0xf6) == 0xf0,
        "attachment is not a valid AAC ADTS payload"
    );
    let info = parse_aac_adts_info(bytes)?;
    let frame_length = (((bytes[3] & 0x03) as usize) << 11)
        | ((bytes[4] as usize) << 3)
        | (((bytes[5] & 0xe0) as usize) >> 5);
    anyhow::ensure!(frame_length >= 7, "AAC ADTS frame length is invalid");
    anyhow::ensure!(
        frame_length <= bytes.len(),
        "AAC ADTS frame length exceeds the provided bytes"
    );
    validate_audio_stream_limits(
        "AAC ADTS",
        Some(info.sample_rate_hz),
        Some(info.channels),
        Some(duration_millis_from_samples(1024, info.sample_rate_hz)?),
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct AacAdtsInfo {
    sample_rate_hz: u32,
    channels: u16,
}

fn parse_aac_adts_info(bytes: &[u8]) -> Result<AacAdtsInfo> {
    const AAC_SAMPLE_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let sample_rate_index = ((bytes[2] >> 2) & 0x0f) as usize;
    let Some(sample_rate_hz) = AAC_SAMPLE_RATES.get(sample_rate_index).copied() else {
        bail!("AAC ADTS sample-rate index is invalid");
    };
    let channels = (((bytes[2] & 0x01) as u16) << 2) | (((bytes[3] >> 6) & 0x03) as u16);
    Ok(AacAdtsInfo {
        sample_rate_hz,
        channels,
    })
}

fn validate_flac_payload(bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(
        bytes.len() >= 42 && bytes.starts_with(b"fLaC"),
        "attachment is not a valid FLAC payload"
    );
    let first_block_header = &bytes[4..8];
    let block_type = first_block_header[0] & 0x7f;
    let block_length = ((first_block_header[1] as usize) << 16)
        | ((first_block_header[2] as usize) << 8)
        | first_block_header[3] as usize;
    anyhow::ensure!(
        block_type == 0,
        "FLAC payload is missing the mandatory STREAMINFO metadata block"
    );
    anyhow::ensure!(
        block_length == 34 && bytes.len() >= 8 + block_length,
        "FLAC STREAMINFO metadata block is truncated"
    );
    let streaminfo = parse_flac_streaminfo(&bytes[8..8 + block_length])?;
    validate_audio_stream_limits(
        "FLAC",
        Some(streaminfo.sample_rate_hz),
        Some(streaminfo.channels),
        streaminfo.duration_ms,
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct FlacStreamInfo {
    sample_rate_hz: u32,
    channels: u16,
    duration_ms: Option<u64>,
}

fn parse_flac_streaminfo(bytes: &[u8]) -> Result<FlacStreamInfo> {
    anyhow::ensure!(
        bytes.len() == 34,
        "FLAC STREAMINFO metadata block has an invalid length"
    );
    let packed = u64::from_be_bytes(bytes[10..18].try_into()?);
    let sample_rate_hz = ((packed >> 44) & 0x000f_ffff) as u32;
    let channels = (((packed >> 41) & 0x07) + 1) as u16;
    let total_samples = packed & 0x0000_000f_ffff_ffff;
    let duration_ms = if total_samples == 0 {
        None
    } else {
        Some(duration_millis_from_samples(total_samples, sample_rate_hz)?)
    };
    Ok(FlacStreamInfo {
        sample_rate_hz,
        channels,
        duration_ms,
    })
}

fn validate_iso_bmff_audio_payload(bytes: &[u8]) -> Result<()> {
    let mut cursor = 0usize;
    let mut saw_ftyp = false;
    let mut saw_mdat = false;
    let mut saw_audio_track = false;
    while let Some(header) = read_iso_bmff_box_header(bytes, cursor, bytes.len())? {
        let box_type = &bytes[header.type_start..header.type_start + 4];
        let payload = &bytes[header.payload_start..header.end];
        match box_type {
            b"ftyp" => saw_ftyp = true,
            b"mdat" if !payload.is_empty() => saw_mdat = true,
            b"moov" => {
                saw_audio_track |= iso_bmff_container_has_audio_handler(payload)?;
            }
            _ => {}
        }
        cursor = header.end;
    }
    anyhow::ensure!(saw_ftyp, "attachment is missing an MP4/M4A ftyp box");
    anyhow::ensure!(saw_mdat, "attachment is missing MP4/M4A media data");
    anyhow::ensure!(
        saw_audio_track,
        "attachment is an MP4/M4A container without an audio track"
    );
    Ok(())
}

struct IsoBmffBoxHeader {
    type_start: usize,
    payload_start: usize,
    end: usize,
}

fn read_iso_bmff_box_header(
    bytes: &[u8],
    cursor: usize,
    limit: usize,
) -> Result<Option<IsoBmffBoxHeader>> {
    if cursor == limit {
        return Ok(None);
    }
    anyhow::ensure!(
        cursor + 8 <= limit,
        "attachment has a truncated MP4/M4A box header"
    );
    let declared_size = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?);
    let type_start = cursor + 4;
    let mut payload_start = cursor + 8;
    let end = match declared_size {
        0 => limit,
        1 => {
            anyhow::ensure!(
                cursor + 16 <= limit,
                "attachment has a truncated MP4/M4A extended box header"
            );
            payload_start = cursor + 16;
            let extended_size = u64::from_be_bytes(bytes[cursor + 8..cursor + 16].try_into()?);
            usize::try_from(extended_size)
                .ok()
                .and_then(|size| cursor.checked_add(size))
                .ok_or_else(|| anyhow!("MP4/M4A box size exceeds addressable memory"))?
        }
        size => cursor + size as usize,
    };
    anyhow::ensure!(
        end <= limit && end >= payload_start,
        "attachment has an invalid MP4/M4A box size"
    );
    Ok(Some(IsoBmffBoxHeader {
        type_start,
        payload_start,
        end,
    }))
}

fn iso_bmff_container_has_audio_handler(bytes: &[u8]) -> Result<bool> {
    let mut cursor = 0usize;
    while let Some(header) = read_iso_bmff_box_header(bytes, cursor, bytes.len())? {
        let box_type = &bytes[header.type_start..header.type_start + 4];
        let payload = &bytes[header.payload_start..header.end];
        match box_type {
            b"hdlr" => {
                if payload.len() >= 12 && &payload[8..12] == b"soun" {
                    return Ok(true);
                }
            }
            b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" => {
                if iso_bmff_container_has_audio_handler(payload)? {
                    return Ok(true);
                }
            }
            _ => {}
        }
        cursor = header.end;
    }
    Ok(false)
}

fn validate_pcm_payload(bytes: &[u8], sample_width_bytes: usize) -> Result<()> {
    anyhow::ensure!(!bytes.is_empty(), "PCM payload is empty");
    anyhow::ensure!(
        sample_width_bytes > 0 && bytes.len() % sample_width_bytes == 0,
        "PCM payload byte length is not aligned to {sample_width_bytes}-byte samples"
    );
    Ok(())
}

fn derive_pdf_asset_artifacts(bytes: &[u8]) -> Result<DerivedAssetArtifacts> {
    let document = PdfDocument::load_mem(bytes).context("failed to parse PDF attachment")?;
    validate_pdf_document_limits(&document)?;
    let pages = document.get_pages().into_keys().collect::<Vec<_>>();
    anyhow::ensure!(
        pages.len() <= MAX_PDF_TEXT_PAGES,
        "PDF attachment has {} pages, exceeding the {} page text extraction limit",
        pages.len(),
        MAX_PDF_TEXT_PAGES
    );
    let text = document
        .extract_text(&pages)
        .context("failed to extract text from PDF attachment")?;
    anyhow::ensure!(
        text.len() <= MAX_PDF_TEXT_BYTES,
        "PDF attachment extracted text is {} bytes, exceeding the {} byte text extraction limit",
        text.len(),
        MAX_PDF_TEXT_BYTES
    );
    Ok(DerivedAssetArtifacts {
        text: Some(text),
        preview: None,
    })
}

fn validate_pdf_document_limits(document: &PdfDocument) -> Result<()> {
    anyhow::ensure!(
        document.objects.len() <= MAX_PDF_OBJECTS,
        "PDF attachment has {} objects, exceeding the {} object parsing limit",
        document.objects.len(),
        MAX_PDF_OBJECTS
    );
    let mut stream_count = 0usize;
    let mut total_stream_bytes = 0usize;
    for object in document.objects.values() {
        let PdfObject::Stream(stream) = object else {
            continue;
        };
        stream_count += 1;
        anyhow::ensure!(
            stream_count <= MAX_PDF_STREAMS,
            "PDF attachment has {stream_count} streams, exceeding the {MAX_PDF_STREAMS} stream parsing limit"
        );
        let stream_bytes = stream.content.len();
        anyhow::ensure!(
            stream_bytes <= MAX_PDF_STREAM_BYTES,
            "PDF attachment stream is {stream_bytes} bytes, exceeding the {MAX_PDF_STREAM_BYTES} byte stream parsing limit"
        );
        total_stream_bytes = total_stream_bytes
            .checked_add(stream_bytes)
            .ok_or_else(|| anyhow!("PDF attachment stream byte accounting overflow"))?;
        anyhow::ensure!(
            total_stream_bytes <= MAX_PDF_TOTAL_STREAM_BYTES,
            "PDF attachment streams total {total_stream_bytes} bytes, exceeding the {MAX_PDF_TOTAL_STREAM_BYTES} byte stream parsing limit"
        );
    }
    Ok(())
}

fn derive_dxf_asset_artifacts(bytes: &[u8]) -> Result<DerivedAssetArtifacts> {
    let analysis = analyze_dxf_attachment(bytes)?;
    Ok(DerivedAssetArtifacts {
        text: Some(analysis.render_summary()),
        preview: analysis
            .render_preview_png()?
            .map(|bytes| ("image/png".to_string(), bytes)),
    })
}

fn analyze_dxf_attachment(bytes: &[u8]) -> Result<DxfAnalysis> {
    validate_dxf_payload(bytes)?;
    let normalized = normalize_text(&String::from_utf8_lossy(bytes));
    let lines = normalized.lines().collect::<Vec<_>>();
    if lines.len() < 2 {
        return Ok(DxfAnalysis::default());
    }

    let mut summary = DxfSummary::default();
    let mut preview = DxfPreviewModel::default();
    let mut current_section: Option<&str> = None;
    let mut current_header: Option<&str> = None;
    let mut current_entity: Option<DxfEntityAccumulator> = None;
    let mut pending_section_name = false;

    let mut index = 0usize;
    while index + 1 < lines.len() {
        let code_text = lines[index].trim();
        let value = lines[index + 1].trim();
        index += 2;

        let Ok(code) = code_text.parse::<i32>() else {
            continue;
        };

        if pending_section_name {
            if code == 2 {
                current_section = Some(value);
                current_header = None;
            }
            pending_section_name = false;
            continue;
        }

        if current_section == Some("HEADER") {
            if code == 9 {
                current_header = Some(value);
                continue;
            }
            match current_header {
                Some("$ACADVER") if code == 1 => summary.version = Some(value.to_string()),
                Some("$DWGCODEPAGE") if code == 3 => summary.code_page = Some(value.to_string()),
                Some("$EXTMIN") => summary.extents_min.apply(code, value),
                Some("$EXTMAX") => summary.extents_max.apply(code, value),
                Some("$LIMMIN") => summary.limits_min.apply(code, value),
                Some("$LIMMAX") => summary.limits_max.apply(code, value),
                _ => {}
            }
        }

        if current_section == Some("ENTITIES") {
            if code == 0 {
                if let Some(entity) = current_entity.take() {
                    summary.push_entity(&entity);
                    preview.push_entity(&entity);
                }
                match value {
                    "ENDSEC" => {
                        current_section = None;
                    }
                    kind => {
                        current_entity = Some(DxfEntityAccumulator::new(kind));
                    }
                }
                continue;
            }
            if let Some(entity) = current_entity.as_mut() {
                entity.apply(code, value);
            }
            continue;
        }

        if code == 0 {
            match value {
                "SECTION" => {
                    pending_section_name = true;
                }
                "ENDSEC" => {
                    current_section = None;
                    current_header = None;
                }
                _ => {}
            }
        }
    }

    if let Some(entity) = current_entity.take() {
        summary.push_entity(&entity);
        preview.push_entity(&entity);
    }

    Ok(DxfAnalysis { summary, preview })
}

#[derive(Default)]
struct DxfAnalysis {
    summary: DxfSummary,
    preview: DxfPreviewModel,
}

impl DxfAnalysis {
    fn render_summary(&self) -> String {
        self.summary.render()
    }

    fn render_preview_png(&self) -> Result<Option<Vec<u8>>> {
        self.preview.render_png()
    }
}

#[derive(Default)]
struct DxfPreviewModel {
    primitives: Vec<DxfPreviewPrimitive>,
    segments: usize,
    truncated: bool,
}

impl DxfPreviewModel {
    fn push_entity(&mut self, entity: &DxfEntityAccumulator) {
        if self.truncated {
            return;
        }
        let Some(primitive) = entity.preview_primitive() else {
            return;
        };
        let segment_count = primitive.segment_count();
        if self.primitives.len() >= MAX_DXF_PREVIEW_PRIMITIVES
            || self.segments.saturating_add(segment_count) > MAX_DXF_PREVIEW_SEGMENTS
        {
            self.truncated = true;
            self.primitives.clear();
            self.segments = 0;
            return;
        }
        self.segments += segment_count;
        self.primitives.push(primitive);
    }

    fn render_png(&self) -> Result<Option<Vec<u8>>> {
        if self.primitives.is_empty() || self.truncated {
            return Ok(None);
        }

        let bounds = self.bounds()?;
        let canvas_size = DXF_PREVIEW_MAX_EDGE_PX;
        let mut image = RgbImage::from_pixel(canvas_size, canvas_size, Rgb(DXF_PREVIEW_BACKGROUND));
        let inner_size = (canvas_size - DXF_PREVIEW_PADDING_PX * 2) as f64;
        let width = (bounds.max_x - bounds.min_x).max(1e-6);
        let height = (bounds.max_y - bounds.min_y).max(1e-6);
        let scale = (inner_size / width).min(inner_size / height);
        let rendered_width = width * scale;
        let rendered_height = height * scale;
        let offset_x =
            DXF_PREVIEW_PADDING_PX as f64 + ((inner_size - rendered_width).max(0.0) / 2.0);
        let offset_y =
            DXF_PREVIEW_PADDING_PX as f64 + ((inner_size - rendered_height).max(0.0) / 2.0);

        for primitive in &self.primitives {
            match primitive {
                DxfPreviewPrimitive::Line { start, end } => {
                    draw_preview_segment(
                        &mut image,
                        map_preview_point(*start, bounds, scale, offset_x, offset_y),
                        map_preview_point(*end, bounds, scale, offset_x, offset_y),
                    );
                }
                DxfPreviewPrimitive::Polyline { points, closed } => {
                    for segment in points.windows(2) {
                        draw_preview_segment(
                            &mut image,
                            map_preview_point(segment[0], bounds, scale, offset_x, offset_y),
                            map_preview_point(segment[1], bounds, scale, offset_x, offset_y),
                        );
                    }
                    if *closed && points.len() > 2 {
                        draw_preview_segment(
                            &mut image,
                            map_preview_point(
                                points[points.len() - 1],
                                bounds,
                                scale,
                                offset_x,
                                offset_y,
                            ),
                            map_preview_point(points[0], bounds, scale, offset_x, offset_y),
                        );
                    }
                }
            }
        }

        let dynamic = DynamicImage::ImageRgb8(image);
        let mut bytes = Vec::new();
        dynamic
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .context("failed to encode DXF preview image")?;
        Ok(Some(bytes))
    }

    fn bounds(&self) -> Result<DxfPreviewBounds> {
        let mut bounds = DxfPreviewBounds::default();
        for primitive in &self.primitives {
            match primitive {
                DxfPreviewPrimitive::Line { start, end } => {
                    bounds.include(*start);
                    bounds.include(*end);
                }
                DxfPreviewPrimitive::Polyline { points, .. } => {
                    for point in points {
                        bounds.include(*point);
                    }
                }
            }
        }
        if bounds.is_empty() {
            bail!("DXF preview did not contain any drawable points");
        }
        Ok(bounds)
    }
}

#[derive(Clone)]
enum DxfPreviewPrimitive {
    Line {
        start: (f64, f64),
        end: (f64, f64),
    },
    Polyline {
        points: Vec<(f64, f64)>,
        closed: bool,
    },
}

impl DxfPreviewPrimitive {
    fn segment_count(&self) -> usize {
        match self {
            DxfPreviewPrimitive::Line { .. } => 1,
            DxfPreviewPrimitive::Polyline { points, closed } => points
                .len()
                .saturating_sub(1)
                .saturating_add(usize::from(*closed && points.len() > 2)),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DxfPreviewBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    initialized: bool,
}

impl DxfPreviewBounds {
    fn include(&mut self, point: (f64, f64)) {
        let (x, y) = point;
        if !self.initialized {
            self.min_x = x;
            self.max_x = x;
            self.min_y = y;
            self.max_y = y;
            self.initialized = true;
            return;
        }
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }

    fn is_empty(&self) -> bool {
        !self.initialized
    }
}

fn map_preview_point(
    point: (f64, f64),
    bounds: DxfPreviewBounds,
    scale: f64,
    offset_x: f64,
    offset_y: f64,
) -> (i32, i32) {
    let x = offset_x + ((point.0 - bounds.min_x) * scale);
    let y = offset_y + ((bounds.max_y - point.1) * scale);
    (x.round() as i32, y.round() as i32)
}

fn draw_preview_segment(image: &mut RgbImage, start: (i32, i32), end: (i32, i32)) {
    let (mut x0, mut y0) = start;
    let (x1, y1) = end;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;

    loop {
        stamp_preview_pixel(image, x0, y0);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let doubled = error * 2;
        if doubled >= dy {
            error += dy;
            x0 += sx;
        }
        if doubled <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

fn stamp_preview_pixel(image: &mut RgbImage, x: i32, y: i32) {
    let radius = DXF_PREVIEW_LINE_THICKNESS_PX.saturating_sub(1);
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 {
                continue;
            }
            let Some(px_u32) = u32::try_from(px).ok() else {
                continue;
            };
            let Some(py_u32) = u32::try_from(py).ok() else {
                continue;
            };
            if px_u32 >= image.width() || py_u32 >= image.height() {
                continue;
            }
            image.put_pixel(px_u32, py_u32, Rgb(DXF_PREVIEW_FOREGROUND));
        }
    }
}

fn validate_dxf_payload(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        bail!("DXF attachment is empty");
    }
    if bytes.contains(&0) {
        bail!("binary DXF attachments are not supported");
    }
    let decoded = String::from_utf8_lossy(bytes);
    if decoded.trim().is_empty() {
        bail!("DXF attachment did not contain readable text");
    }
    let tokens = decoded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(128)
        .collect::<Vec<_>>();
    let mut saw_group_code = false;
    let mut saw_structure_marker = false;
    for pair in tokens.windows(2) {
        if pair[0].parse::<i32>().is_ok() {
            saw_group_code = true;
            if matches!(
                pair[1],
                "SECTION" | "HEADER" | "TABLE" | "BLOCK" | "ENTITIES" | "EOF"
            ) {
                saw_structure_marker = true;
                break;
            }
        }
    }
    if !saw_group_code || !saw_structure_marker {
        bail!("DXF attachment did not contain recognizable DXF structure markers");
    }
    if !decoded.contains("EOF") {
        bail!("DXF attachment did not contain an EOF marker");
    }
    Ok(())
}

#[derive(Default)]
struct DxfSummary {
    version: Option<String>,
    code_page: Option<String>,
    extents_min: DxfPoint3,
    extents_max: DxfPoint3,
    limits_min: DxfPoint3,
    limits_max: DxfPoint3,
    entity_counts: BTreeMap<String, usize>,
    explicit_dimensions: Vec<String>,
    text_labels: Vec<String>,
    geometry_samples: Vec<String>,
}

impl DxfSummary {
    fn push_entity(&mut self, entity: &DxfEntityAccumulator) {
        *self.entity_counts.entry(entity.kind.clone()).or_default() += 1;
        if let Some(measurement) = entity.dimension_summary() {
            if self.explicit_dimensions.len() < 32 {
                self.explicit_dimensions.push(measurement);
            }
        }
        if let Some(label) = entity.text_label() {
            if self.text_labels.len() < 32 {
                self.text_labels.push(label);
            }
        }
        if let Some(sample) = entity.geometry_sample() {
            if self.geometry_samples.len() < 24 {
                self.geometry_samples.push(sample);
            }
        }
    }

    fn render(&self) -> String {
        let mut lines = vec!["DXF summary".to_string()];
        if let Some(version) = &self.version {
            lines.push(format!("Version: {version}"));
        }
        if let Some(code_page) = &self.code_page {
            lines.push(format!("Code page: {code_page}"));
        }
        if let (Some(min), Some(max)) = (self.extents_min.render(), self.extents_max.render()) {
            lines.push(format!("Extents: {min} -> {max}"));
        }
        if let (Some(min), Some(max)) = (self.limits_min.render(), self.limits_max.render()) {
            lines.push(format!("Limits: {min} -> {max}"));
        }
        if !self.entity_counts.is_empty() {
            let counts = self
                .entity_counts
                .iter()
                .map(|(kind, count)| format!("{kind}={count}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("Entity counts: {counts}"));
        }
        if !self.explicit_dimensions.is_empty() {
            lines.push(format!(
                "Explicit dimensions: {}",
                self.explicit_dimensions.join(", ")
            ));
        }
        if !self.text_labels.is_empty() {
            lines.push(format!("Text labels: {}", self.text_labels.join("; ")));
        }
        if !self.geometry_samples.is_empty() {
            lines.push("Geometry samples:".to_string());
            lines.extend(
                self.geometry_samples
                    .iter()
                    .map(|sample| format!("- {sample}")),
            );
        }
        lines.join("\n")
    }
}

#[derive(Clone, Copy, Default)]
struct DxfPoint3 {
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
}

impl DxfPoint3 {
    fn apply(&mut self, code: i32, value: &str) {
        let Some(number) = parse_finite_f64(value) else {
            return;
        };
        match code {
            10 => self.x = Some(number),
            20 => self.y = Some(number),
            30 => self.z = Some(number),
            _ => {}
        }
    }

    fn render(&self) -> Option<String> {
        let x = self.x?;
        let y = self.y?;
        let z = self.z.unwrap_or(0.0);
        Some(format!(
            "({}, {}, {})",
            format_number(x),
            format_number(y),
            format_number(z)
        ))
    }
}

#[derive(Default)]
struct DxfEntityAccumulator {
    kind: String,
    layer: Option<String>,
    closed: bool,
    polyline_points: Vec<(f64, f64)>,
    pending_poly_x: Option<f64>,
    line_start_x: Option<f64>,
    line_start_y: Option<f64>,
    line_end_x: Option<f64>,
    line_end_y: Option<f64>,
    insert_name: Option<String>,
    insert_x: Option<f64>,
    insert_y: Option<f64>,
    text_fragments: Vec<String>,
    text_x: Option<f64>,
    text_y: Option<f64>,
    dimension_measurement: Option<f64>,
    dimension_text: Option<String>,
}

impl DxfEntityAccumulator {
    fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            ..Self::default()
        }
    }

    fn apply(&mut self, code: i32, value: &str) {
        match code {
            8 => {
                if !value.is_empty() {
                    self.layer = Some(value.to_string());
                }
            }
            70 if self.kind == "LWPOLYLINE" => {
                self.closed = value
                    .parse::<i32>()
                    .map(|flags| flags & 1 == 1)
                    .unwrap_or(false);
            }
            10 if self.kind == "LWPOLYLINE" => {
                self.pending_poly_x = parse_finite_f64(value);
            }
            20 if self.kind == "LWPOLYLINE" => {
                if let (Some(x), Some(y)) = (self.pending_poly_x.take(), parse_finite_f64(value)) {
                    if self.polyline_points.len() < MAX_DXF_POLYLINE_POINTS {
                        self.polyline_points.push((x, y));
                    }
                }
            }
            10 if self.kind == "LINE" => self.line_start_x = parse_finite_f64(value),
            20 if self.kind == "LINE" => self.line_start_y = parse_finite_f64(value),
            11 if self.kind == "LINE" => self.line_end_x = parse_finite_f64(value),
            21 if self.kind == "LINE" => self.line_end_y = parse_finite_f64(value),
            2 if self.kind == "INSERT" => {
                if !value.is_empty() {
                    self.insert_name = Some(value.to_string());
                }
            }
            10 if self.kind == "INSERT" => self.insert_x = parse_finite_f64(value),
            20 if self.kind == "INSERT" => self.insert_y = parse_finite_f64(value),
            1 | 3 if matches!(self.kind.as_str(), "TEXT" | "MTEXT") => {
                if !value.is_empty() {
                    self.text_fragments.push(value.to_string());
                }
            }
            10 if matches!(self.kind.as_str(), "TEXT" | "MTEXT") => {
                self.text_x = parse_finite_f64(value)
            }
            20 if matches!(self.kind.as_str(), "TEXT" | "MTEXT") => {
                self.text_y = parse_finite_f64(value)
            }
            42 if self.kind == "DIMENSION" => {
                self.dimension_measurement = parse_finite_f64(value);
            }
            1 if self.kind == "DIMENSION" && !value.is_empty() => {
                self.dimension_text = Some(value.to_string());
            }
            _ => {}
        }
    }

    fn dimension_summary(&self) -> Option<String> {
        if self.kind != "DIMENSION" {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(measurement) = self.dimension_measurement {
            parts.push(format_number(measurement));
        }
        if let Some(text) = &self.dimension_text {
            if text.trim().is_empty() || text.trim() == "<>" {
                if parts.is_empty() {
                    return None;
                }
            } else {
                parts.push(text.trim().to_string());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" / "))
        }
    }

    fn text_label(&self) -> Option<String> {
        if !matches!(self.kind.as_str(), "TEXT" | "MTEXT") {
            return None;
        }
        let text = self
            .text_fragments
            .iter()
            .map(String::as_str)
            .collect::<String>()
            .trim()
            .to_string();
        if text.is_empty() {
            return None;
        }
        match (self.text_x, self.text_y) {
            (Some(x), Some(y)) => Some(format!(
                "{} @ ({}, {})",
                text,
                format_number(x),
                format_number(y)
            )),
            _ => Some(text),
        }
    }

    fn geometry_sample(&self) -> Option<String> {
        match self.kind.as_str() {
            "LWPOLYLINE" => self.polyline_sample(),
            "LINE" => self.line_sample(),
            "INSERT" => self.insert_sample(),
            _ => None,
        }
    }

    fn preview_primitive(&self) -> Option<DxfPreviewPrimitive> {
        match self.kind.as_str() {
            "LINE" => {
                let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
                    self.line_start_x,
                    self.line_start_y,
                    self.line_end_x,
                    self.line_end_y,
                ) else {
                    return None;
                };
                Some(DxfPreviewPrimitive::Line {
                    start: (x1, y1),
                    end: (x2, y2),
                })
            }
            "LWPOLYLINE" if self.polyline_points.len() >= 2 => {
                Some(DxfPreviewPrimitive::Polyline {
                    points: self.polyline_points.clone(),
                    closed: self.closed,
                })
            }
            _ => None,
        }
    }

    fn polyline_sample(&self) -> Option<String> {
        if self.polyline_points.is_empty() {
            return None;
        }
        let layer = self.layer.as_deref().unwrap_or("default");
        let mut min_x = self.polyline_points[0].0;
        let mut min_y = self.polyline_points[0].1;
        let mut max_x = self.polyline_points[0].0;
        let mut max_y = self.polyline_points[0].1;
        for (x, y) in &self.polyline_points {
            min_x = min_x.min(*x);
            min_y = min_y.min(*y);
            max_x = max_x.max(*x);
            max_y = max_y.max(*y);
        }
        let sample_points = self
            .polyline_points
            .iter()
            .take(8)
            .map(|(x, y)| format!("({}, {})", format_number(*x), format_number(*y)))
            .collect::<Vec<_>>()
            .join(" ");
        let suffix = if self.polyline_points.len() > 8 {
            " ..."
        } else {
            ""
        };
        Some(format!(
            "LWPOLYLINE layer={layer} closed={} vertices={} bbox=({}, {}) -> ({}, {}) points={}{}",
            self.closed,
            self.polyline_points.len(),
            format_number(min_x),
            format_number(min_y),
            format_number(max_x),
            format_number(max_y),
            sample_points,
            suffix
        ))
    }

    fn line_sample(&self) -> Option<String> {
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            self.line_start_x,
            self.line_start_y,
            self.line_end_x,
            self.line_end_y,
        ) else {
            return None;
        };
        let layer = self.layer.as_deref().unwrap_or("default");
        Some(format!(
            "LINE layer={layer} from=({}, {}) to=({}, {})",
            format_number(x1),
            format_number(y1),
            format_number(x2),
            format_number(y2)
        ))
    }

    fn insert_sample(&self) -> Option<String> {
        let name = self.insert_name.as_deref()?;
        let x = self.insert_x?;
        let y = self.insert_y?;
        let layer = self.layer.as_deref().unwrap_or("default");
        Some(format!(
            "INSERT layer={layer} name={name} at=({}, {})",
            format_number(x),
            format_number(y)
        ))
    }
}

fn format_number(value: f64) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn parse_finite_f64(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    use image::{ColorType, ImageBuffer, ImageEncoder, Rgb, codecs::jpeg::JpegEncoder};
    use lopdf::{Document, Object, Stream, dictionary};
    use tempfile::TempDir;

    fn sample_pdf_bytes(text: &str) -> Result<Vec<u8>> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = document.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            }
        });
        let content = format!("BT\n/F1 18 Tf\n72 96 Td\n({text}) Tj\nET");
        let content_id = document.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        document.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 300.into(), 144.into()],
                "Contents" => content_id,
                "Resources" => resources_id,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .context("failed to build PDF test fixture")?;
        Ok(bytes)
    }

    fn pdf_with_extra_null_objects(extra_objects: usize) -> Result<Vec<u8>> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.new_object_id();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        document.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        for _ in 0..extra_objects {
            document.add_object(Object::Null);
        }
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .context("failed to build oversized PDF test fixture")?;
        Ok(bytes)
    }

    fn pdf_with_blank_pages(page_count: usize) -> Result<Vec<u8>> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let mut kids = Vec::with_capacity(page_count);
        for _ in 0..page_count {
            let page_id = document.new_object_id();
            kids.push(page_id.into());
            document.objects.insert(
                page_id,
                Object::Dictionary(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                }),
            );
        }
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => page_count as i64,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .context("failed to build many-page PDF test fixture")?;
        Ok(bytes)
    }

    fn pdf_with_extra_stream_objects(stream_count: usize) -> Result<Vec<u8>> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.new_object_id();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        document.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        for _ in 0..stream_count {
            document.add_object(Stream::new(dictionary! {}, vec![0]));
        }
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .context("failed to build many-stream PDF test fixture")?;
        Ok(bytes)
    }

    fn sample_png_bytes() -> Result<Vec<u8>> {
        let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(4, 4, Rgb([255, 0, 0]));
        let mut cursor = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut cursor, image::ImageFormat::Png)
            .context("failed to build PNG test fixture")?;
        Ok(cursor.into_inner())
    }

    fn sample_jpeg_bytes() -> Result<Vec<u8>> {
        let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(4, 4, Rgb([0, 0, 255]));
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 90)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ColorType::Rgb8.into(),
            )
            .context("failed to build JPEG test fixture")?;
        Ok(bytes)
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

    fn jpeg_header_with_dimensions(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xff, 0xd8, 0xff, 0xc0]);
        bytes.extend_from_slice(&17u16.to_be_bytes());
        bytes.push(8);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.push(3);
        bytes.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        bytes
    }

    fn sample_csv_bytes() -> Vec<u8> {
        b"room,width,height\nDining Room,6.5,3.4\nOther,9.4,2.9\n".to_vec()
    }

    fn sample_wav_bytes() -> Vec<u8> {
        let samples = [0i16, 1024, -1024, 2048];
        let data = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let fmt_chunk_size = 16u32;
        let data_chunk_size = data.len() as u32;
        let riff_size = 4 + (8 + fmt_chunk_size) + (8 + data_chunk_size);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&fmt_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&(16_000u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&data);
        bytes
    }

    fn sample_empty_wav_bytes() -> Vec<u8> {
        let fmt_chunk_size = 16u32;
        let data_chunk_size = 0u32;
        let riff_size = 4 + (8 + fmt_chunk_size) + (8 + data_chunk_size);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&fmt_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&(16_000u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_chunk_size.to_le_bytes());
        bytes
    }

    fn wav_with_u16_at(mut bytes: Vec<u8>, offset: usize, value: u16) -> Vec<u8> {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    fn wav_with_u32_at(mut bytes: Vec<u8>, offset: usize, value: u32) -> Vec<u8> {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    fn sample_wav_with_unaligned_data() -> Vec<u8> {
        let mut bytes = sample_wav_bytes();
        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().expect("riff size")) + 2;
        let data_size = u32::from_le_bytes(bytes[40..44].try_into().expect("data size")) + 1;
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes[40..44].copy_from_slice(&data_size.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes
    }

    fn sample_wav_with_duplicate_fmt() -> Vec<u8> {
        let mut bytes = sample_wav_bytes();
        let duplicate_fmt = bytes[12..36].to_vec();
        bytes.splice(36..36, duplicate_fmt);
        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().expect("riff size")) + 24;
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    fn sample_extensible_wav_bytes(subformat: [u8; 16]) -> Vec<u8> {
        let samples = [0i16, 1024, -1024, 2048];
        let data = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let fmt_chunk_size = 40u32;
        let data_chunk_size = data.len() as u32;
        let riff_size = 4 + (8 + fmt_chunk_size) + (8 + data_chunk_size);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&fmt_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&0xfffeu16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&(16_000u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&22u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&subformat);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&data);
        bytes
    }

    fn sample_streaming_length_wav_bytes() -> Vec<u8> {
        let samples = [0i16, 1024, -1024, 2048];
        let data = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let fmt_chunk_size = 16u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&fmt_chunk_size.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&24_000u32.to_le_bytes());
        bytes.extend_from_slice(&(24_000u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&data);
        bytes
    }

    fn pcm_wav_subformat_guid() -> [u8; 16] {
        [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
            0x9b, 0x71,
        ]
    }

    fn sample_webm_bytes() -> Vec<u8> {
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

    fn sample_mp3_bytes() -> Vec<u8> {
        let mut bytes = vec![0xff, 0xfb, 0x90, 0x64];
        bytes.resize(417, 0);
        bytes
    }

    fn sample_ogg_opus_bytes() -> Vec<u8> {
        let mut bytes = vec![0; 27];
        bytes[..4].copy_from_slice(b"OggS");
        bytes[26] = 1;
        bytes.push(19);
        bytes.extend_from_slice(b"OpusHead");
        bytes.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes
    }

    fn sample_aac_bytes() -> Vec<u8> {
        vec![0xff, 0xf1, 0x50, 0x80, 0x01, 0x7f, 0xfc, 0, 1, 2, 3]
    }

    fn sample_flac_bytes() -> Vec<u8> {
        sample_flac_with_streaminfo(44_100, 1, 16, 44_100)
    }

    fn sample_flac_with_streaminfo(
        sample_rate_hz: u32,
        channels: u16,
        bits_per_sample: u16,
        total_samples: u64,
    ) -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        let mut streaminfo = [0u8; 34];
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
        let packed = ((u64::from(sample_rate_hz) & 0x000f_ffff) << 44)
            | ((u64::from(channels.saturating_sub(1)) & 0x07) << 41)
            | ((u64::from(bits_per_sample.saturating_sub(1)) & 0x1f) << 36)
            | (total_samples & 0x0000_000f_ffff_ffff);
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        bytes.extend_from_slice(&streaminfo);
        bytes
    }

    fn sample_m4a_bytes() -> Vec<u8> {
        sample_iso_bmff_bytes(*b"soun")
    }

    fn sample_video_mp4_bytes() -> Vec<u8> {
        sample_iso_bmff_bytes(*b"vide")
    }

    fn sample_iso_bmff_bytes(handler_type: [u8; 4]) -> Vec<u8> {
        let ftyp = sample_iso_box(*b"ftyp", b"M4A \0\0\0\0M4A ".to_vec());
        let hdlr = sample_iso_box(
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
        let mdia = sample_iso_box(*b"mdia", hdlr);
        let trak = sample_iso_box(*b"trak", mdia);
        let moov = sample_iso_box(*b"moov", trak);
        let mdat = sample_iso_box(*b"mdat", vec![0, 1, 2, 3]);
        [ftyp, moov, mdat].concat()
    }

    fn sample_iso_box(kind: [u8; 4], payload: Vec<u8>) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).expect("test ISO box size");
        let mut bytes = Vec::with_capacity(payload.len() + 8);
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(&kind);
        bytes.extend_from_slice(&payload);
        bytes
    }

    fn sample_pcm16_bytes() -> Vec<u8> {
        vec![0, 0, 1, 0, 255, 255, 0, 128]
    }

    fn sample_pcm24_bytes() -> Vec<u8> {
        vec![0, 0, 0, 1, 0, 0]
    }

    fn has_file_name_with_prefix(dir: &Path, prefix: &str) -> Result<bool> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if file_name.starts_with(prefix) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn assert_asset_integrity_error(error: &anyhow::Error) {
        assert!(
            error.to_string().contains("asset integrity mismatch"),
            "unexpected error: {error:#}"
        );
    }

    fn sample_dxf_bytes() -> Vec<u8> {
        [
            "0",
            "SECTION",
            "2",
            "HEADER",
            "9",
            "$ACADVER",
            "1",
            "AC1015",
            "9",
            "$DWGCODEPAGE",
            "3",
            "ANSI_1252",
            "9",
            "$EXTMIN",
            "10",
            "0.0",
            "20",
            "0.0",
            "30",
            "0.0",
            "9",
            "$EXTMAX",
            "10",
            "10.0",
            "20",
            "5.0",
            "30",
            "0.0",
            "0",
            "ENDSEC",
            "0",
            "SECTION",
            "2",
            "ENTITIES",
            "0",
            "LWPOLYLINE",
            "8",
            "Walls",
            "70",
            "1",
            "10",
            "0.0",
            "20",
            "0.0",
            "10",
            "10.0",
            "20",
            "0.0",
            "10",
            "10.0",
            "20",
            "5.0",
            "10",
            "0.0",
            "20",
            "5.0",
            "0",
            "DIMENSION",
            "8",
            "Dims",
            "42",
            "3.2",
            "1",
            "3.20",
            "0",
            "TEXT",
            "8",
            "Labels",
            "1",
            "Dining Room",
            "10",
            "1.0",
            "20",
            "2.0",
            "0",
            "ENDSEC",
            "0",
            "EOF",
        ]
        .join("\n")
        .into_bytes()
    }

    fn dxf_with_large_extent_line() -> Vec<u8> {
        [
            "0",
            "SECTION",
            "2",
            "ENTITIES",
            "0",
            "LINE",
            "8",
            "Huge",
            "10",
            "0",
            "20",
            "0",
            "11",
            "1000000000",
            "21",
            "1000000000",
            "0",
            "ENDSEC",
            "0",
            "EOF",
        ]
        .join("\n")
        .into_bytes()
    }

    fn dxf_with_many_lines(line_count: usize) -> Vec<u8> {
        let mut lines = vec![
            "0".to_string(),
            "SECTION".to_string(),
            "2".to_string(),
            "ENTITIES".to_string(),
        ];
        for index in 0..line_count {
            let y = index.to_string();
            lines.extend([
                "0".to_string(),
                "LINE".to_string(),
                "8".to_string(),
                "Stress".to_string(),
                "10".to_string(),
                "0".to_string(),
                "20".to_string(),
                y.clone(),
                "11".to_string(),
                "1".to_string(),
                "21".to_string(),
                y,
            ]);
        }
        lines.extend([
            "0".to_string(),
            "ENDSEC".to_string(),
            "0".to_string(),
            "EOF".to_string(),
        ]);
        lines.join("\n").into_bytes()
    }

    #[test]
    fn imports_supported_assets_and_renders_documents() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let text = store.import_bytes("note.txt", None, b"hello from text asset")?;
        let csv = store.import_bytes("rooms.csv", None, &sample_csv_bytes())?;
        let markdown = store.import_bytes("note.md", None, b"# Title\n\nmarkdown asset")?;
        let dxf = store.import_bytes("plan.dxf", None, &sample_dxf_bytes())?;
        let json = store.import_bytes("note.json", None, br#"{"answer":"ok"}"#)?;
        let pdf = store.import_bytes(
            "note.pdf",
            None,
            &sample_pdf_bytes("KHEISH PDF ASSET TEXT")?,
        )?;
        let png = store.import_bytes("pixel.png", None, &sample_png_bytes()?)?;
        let jpeg = store.import_bytes("pixel.jpg", None, &sample_jpeg_bytes()?)?;
        let wav = store.import_bytes("sample.wav", None, &sample_wav_bytes())?;
        let webm = store.import_bytes("sample.webm", None, &sample_webm_bytes())?;
        let mp3 = store.import_bytes("sample.mp3", Some("audio/mpeg"), &sample_mp3_bytes())?;
        let opus =
            store.import_bytes("sample.opus", Some("audio/opus"), &sample_ogg_opus_bytes())?;
        let aac = store.import_bytes("sample.aac", Some("audio/aac"), &sample_aac_bytes())?;
        let flac = store.import_bytes("sample.flac", Some("audio/flac"), &sample_flac_bytes())?;
        let m4a = store.import_bytes("sample.m4a", Some("audio/m4a"), &sample_m4a_bytes())?;
        let pcm = store.import_bytes("sample.pcm", Some("audio/pcm"), &sample_pcm16_bytes())?;
        let l24 = store.import_bytes("sample-l24.pcm", Some("audio/l24"), &sample_pcm24_bytes())?;

        assert_eq!(text.media_type, "text/plain");
        assert_eq!(csv.media_type, "text/csv");
        assert_eq!(markdown.media_type, "text/markdown");
        assert_eq!(dxf.media_type, "application/dxf");
        assert_eq!(json.media_type, "application/json");
        assert_eq!(pdf.media_type, "application/pdf");
        assert_eq!(png.media_type, "image/png");
        assert_eq!(jpeg.media_type, "image/jpeg");
        assert_eq!(wav.media_type, "audio/wav");
        assert_eq!(webm.media_type, "audio/webm");
        assert_eq!(mp3.media_type, "audio/mpeg");
        assert_eq!(opus.media_type, "audio/opus");
        assert_eq!(aac.media_type, "audio/aac");
        assert_eq!(flac.media_type, "audio/flac");
        assert_eq!(m4a.media_type, "audio/m4a");
        assert_eq!(pcm.media_type, "audio/pcm");
        assert_eq!(l24.media_type, "audio/l24");
        assert!(png.text_uri.is_none());
        assert!(jpeg.text_uri.is_none());
        assert!(wav.text_uri.is_none());
        assert!(webm.text_uri.is_none());
        assert!(mp3.text_uri.is_none());
        assert!(opus.text_uri.is_none());
        assert!(aac.text_uri.is_none());
        assert!(flac.text_uri.is_none());
        assert!(m4a.text_uri.is_none());
        assert!(pcm.text_uri.is_none());
        assert!(l24.text_uri.is_none());
        assert!(dxf.preview_image_uri.is_some());
        assert_eq!(dxf.preview_image_media_type.as_deref(), Some("image/png"));

        let rendered = [
            store.render_asset_transcript_part(&text)?,
            store.render_asset_transcript_part(&csv)?,
            store.render_asset_transcript_part(&markdown)?,
            store.render_asset_transcript_part(&dxf)?,
            store.render_asset_transcript_part(&json)?,
            store.render_asset_transcript_part(&pdf)?,
            store.render_asset_transcript_part(&png)?,
            store.render_asset_transcript_part(&jpeg)?,
            store.render_asset_transcript_part(&wav)?,
            store.render_asset_transcript_part(&webm)?,
            store.render_asset_transcript_part(&mp3)?,
            store.render_asset_transcript_part(&opus)?,
            store.render_asset_transcript_part(&aac)?,
            store.render_asset_transcript_part(&flac)?,
            store.render_asset_transcript_part(&m4a)?,
            store.render_asset_transcript_part(&pcm)?,
            store.render_asset_transcript_part(&l24)?,
        ]
        .join("\n\n");
        assert!(rendered.contains("hello from text asset"));
        assert!(rendered.contains("Dining Room,6.5,3.4"));
        assert!(rendered.contains("DXF summary"));
        assert!(rendered.contains("Explicit dimensions: 3.2 / 3.20"));
        assert!(rendered.contains("Text labels: Dining Room @ (1, 2)"));
        assert!(rendered.contains("\"answer\": \"ok\""));
        assert!(rendered.contains("KHEISH PDF ASSET TEXT"));
        assert!(rendered.contains("Attached image: pixel.png (image/png)"));
        assert!(rendered.contains("Attached file: sample.wav (audio/wav)"));
        assert!(rendered.contains("Attached file: sample.webm (audio/webm)"));
        assert!(rendered.contains("Attached file: sample.mp3 (audio/mpeg)"));
        assert!(rendered.contains("Attached file: sample.opus (audio/opus)"));
        assert!(rendered.contains("Attached file: sample.aac (audio/aac)"));
        assert!(rendered.contains("Attached file: sample.flac (audio/flac)"));
        assert!(rendered.contains("Attached file: sample.m4a (audio/m4a)"));
        assert!(rendered.contains("Attached file: sample.pcm (audio/pcm)"));
        assert!(rendered.contains("Attached file: sample-l24.pcm (audio/l24)"));
        Ok(())
    }

    #[test]
    fn import_accepts_declared_media_type_parameters() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        let webm = store.import_bytes(
            "sample.webm",
            Some("audio/webm;codecs=opus"),
            &sample_webm_bytes(),
        )?;
        let csv = store.import_bytes(
            "rooms.csv",
            Some("text/csv; charset=utf-8"),
            &sample_csv_bytes(),
        )?;

        assert_eq!(webm.media_type, "audio/webm");
        assert_eq!(csv.media_type, "text/csv");
        Ok(())
    }

    #[test]
    fn audio_import_rejects_declared_payload_mismatches() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        for (file_name, media_type, bytes, expected) in [
            (
                "fake.mp3",
                "audio/mpeg",
                b"fake-mp3".as_slice(),
                "valid MP3",
            ),
            (
                "fake.mpga",
                "audio/mpga",
                b"fake-mpga".as_slice(),
                "valid MP3",
            ),
            (
                "fake.opus",
                "audio/opus",
                b"fake-opus".as_slice(),
                "Ogg Opus",
            ),
            (
                "fake.aac",
                "audio/aac",
                b"fake-aac".as_slice(),
                "valid AAC ADTS",
            ),
            (
                "fake.flac",
                "audio/flac",
                b"fake-flac".as_slice(),
                "valid FLAC",
            ),
            ("fake.mp4", "audio/mp4", b"fake-mp4".as_slice(), "MP4/M4A"),
            ("fake.m4a", "audio/m4a", b"fake-m4a".as_slice(), "MP4/M4A"),
        ] {
            let error = store
                .import_bytes(file_name, Some(media_type), bytes)
                .expect_err("invalid audio payload should be rejected");
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {media_type}: {error}"
            );
        }

        let pcm_error = store
            .import_bytes("bad.pcm", Some("audio/l16"), &[0])
            .expect_err("misaligned PCM should be rejected");
        assert!(
            pcm_error.to_string().contains("aligned"),
            "unexpected error: {pcm_error}"
        );
        let pcm24_error = store
            .import_bytes("bad-l24.pcm", Some("audio/l24"), &[0, 1])
            .expect_err("misaligned 24-bit PCM should be rejected");
        assert!(
            pcm24_error.to_string().contains("3-byte"),
            "unexpected error: {pcm24_error}"
        );
        Ok(())
    }

    #[test]
    fn audio_import_rejects_header_valid_truncated_payloads() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        for (file_name, media_type, bytes, expected) in [
            (
                "truncated.mp3",
                "audio/mpeg",
                vec![0xff, 0xfb, 0x90, 0x64],
                "MP3 audio frame is truncated",
            ),
            (
                "truncated.opus",
                "audio/opus",
                {
                    let mut bytes = sample_ogg_opus_bytes();
                    bytes.pop();
                    bytes
                },
                "Ogg Opus page body is truncated",
            ),
            (
                "truncated.flac",
                "audio/flac",
                b"fLaC\x80\x00\x00\x00".to_vec(),
                "valid FLAC",
            ),
            (
                "empty.wav",
                "audio/wav",
                sample_empty_wav_bytes(),
                "non-empty audio data",
            ),
        ] {
            let error = store
                .import_bytes(file_name, Some(media_type), &bytes)
                .expect_err("header-valid but incomplete audio should be rejected");
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {media_type}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn audio_import_rejects_incoherent_declared_audio_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        let mut zero_channel_opus = sample_ogg_opus_bytes();
        zero_channel_opus[28 + 9] = 0;

        let mut zero_channel_aac = sample_aac_bytes();
        zero_channel_aac[3] &= 0x3f;

        for (file_name, media_type, bytes, expected) in [
            (
                "zero-rate.flac",
                "audio/flac",
                sample_flac_with_streaminfo(0, 1, 16, 0),
                "sample rate",
            ),
            (
                "too-long.flac",
                "audio/flac",
                sample_flac_with_streaminfo(8_000, 1, 16, 8_000 * 60 * 31),
                "audio duration",
            ),
            (
                "zero-channel.opus",
                "audio/opus",
                zero_channel_opus,
                "channel count",
            ),
            (
                "zero-channel.aac",
                "audio/aac",
                zero_channel_aac,
                "channel count",
            ),
        ] {
            let error = store
                .import_bytes(file_name, Some(media_type), &bytes)
                .expect_err("incoherent audio metadata should be rejected");
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {file_name}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn audio_import_rejects_incoherent_wav_format_fields() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        for (file_name, bytes, expected) in [
            (
                "unsupported-format.wav",
                wav_with_u16_at(sample_wav_bytes(), 20, 6),
                "not supported",
            ),
            (
                "zero-channels.wav",
                wav_with_u16_at(sample_wav_bytes(), 22, 0),
                "channel count",
            ),
            (
                "too-many-channels.wav",
                wav_with_u16_at(sample_wav_bytes(), 22, 9),
                "channel count",
            ),
            (
                "bad-sample-rate.wav",
                wav_with_u32_at(sample_wav_bytes(), 24, 1),
                "sample rate",
            ),
            (
                "bad-bits.wav",
                wav_with_u16_at(sample_wav_bytes(), 34, 12),
                "bits per sample",
            ),
            (
                "bad-block-align.wav",
                wav_with_u16_at(sample_wav_bytes(), 32, 4),
                "block_align",
            ),
            (
                "bad-byte-rate.wav",
                wav_with_u32_at(sample_wav_bytes(), 28, 1),
                "byte_rate",
            ),
            (
                "unaligned-data.wav",
                sample_wav_with_unaligned_data(),
                "data byte length",
            ),
            (
                "duplicate-fmt.wav",
                sample_wav_with_duplicate_fmt(),
                "multiple fmt chunks",
            ),
            (
                "unknown-extensible.wav",
                sample_extensible_wav_bytes([0; 16]),
                "extensible subformat",
            ),
        ] {
            let error = store
                .import_bytes(file_name, Some("audio/wav"), &bytes)
                .expect_err("incoherent WAV fmt/data should be rejected");
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {file_name}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn audio_import_accepts_wav_extensible_pcm() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        let asset = store.import_bytes(
            "extensible.wav",
            Some("audio/wav"),
            &sample_extensible_wav_bytes(pcm_wav_subformat_guid()),
        )?;
        assert_eq!(asset.media_type, "audio/wav");
        Ok(())
    }

    #[test]
    fn audio_import_accepts_streaming_length_wav() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        let asset = store.import_bytes(
            "streaming-length.wav",
            Some("audio/wav"),
            &sample_streaming_length_wav_bytes(),
        )?;
        assert_eq!(asset.media_type, "audio/wav");
        Ok(())
    }

    #[test]
    fn audio_import_rejects_video_only_mp4_declared_as_audio() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        let error = store
            .import_bytes("clip.mp4", Some("audio/mp4"), &sample_video_mp4_bytes())
            .expect_err("video-only MP4 should not import as audio");
        assert!(
            error.to_string().contains("without an audio track"),
            "unexpected error: {error}"
        );

        let audio = store.import_bytes("call.m4a", Some("audio/m4a"), &sample_m4a_bytes())?;
        assert_eq!(audio.media_type, "audio/m4a");
        Ok(())
    }

    #[test]
    fn image_import_rejects_oversized_dimensions_before_decode() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;

        let png_error = store
            .import_bytes(
                "huge.png",
                Some("image/png"),
                &png_header_with_dimensions(MAX_SOURCE_IMAGE_EDGE_PX + 1, 1),
            )
            .expect_err("oversized PNG header should be rejected");
        assert!(
            png_error.to_string().contains("source edge limit"),
            "unexpected error: {png_error}"
        );

        let jpeg_error = store
            .import_bytes(
                "huge.jpg",
                Some("image/jpeg"),
                &jpeg_header_with_dimensions(10_000, 6_000),
            )
            .expect_err("oversized JPEG header should be rejected");
        assert!(
            jpeg_error.to_string().contains("source pixel limit"),
            "unexpected error: {jpeg_error}"
        );
        Ok(())
    }

    #[test]
    fn image_decode_respects_explicit_allocation_limit() -> Result<()> {
        let bytes = sample_png_bytes()?;
        let error = decode_image_with_limits("image/png", &bytes, MAX_SOURCE_IMAGE_EDGE_PX, 1)
            .expect_err("decode should respect a tiny allocation limit");
        assert!(
            format!("{error:#}").contains("limit") || format!("{error:#}").contains("memory"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn pdf_import_rejects_excessive_object_count_before_text_extraction() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let bytes = pdf_with_extra_null_objects(MAX_PDF_OBJECTS + 1)?;

        let error = store
            .import_bytes("object-bomb.pdf", Some("application/pdf"), &bytes)
            .expect_err("PDF object bomb should be rejected");
        assert!(
            error.to_string().contains("object parsing limit"),
            "unexpected error: {error}"
        );
        assert!(
            !has_file_name_with_prefix(&temp.path().join("assets").join("raw"), "asset-1.")?,
            "failed PDF derivation should not leave a raw payload behind"
        );
        Ok(())
    }

    #[test]
    fn pdf_import_rejects_excessive_page_count_before_text_extraction() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let bytes = pdf_with_blank_pages(MAX_PDF_TEXT_PAGES + 1)?;

        let error = store
            .import_bytes("many-pages.pdf", Some("application/pdf"), &bytes)
            .expect_err("PDF with too many pages should be rejected");
        assert!(
            error.to_string().contains("page text extraction limit"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[test]
    fn pdf_import_rejects_excessive_stream_count_before_text_extraction() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let bytes = pdf_with_extra_stream_objects(MAX_PDF_STREAMS + 1)?;

        let error = store
            .import_bytes("many-streams.pdf", Some("application/pdf"), &bytes)
            .expect_err("PDF with too many streams should be rejected");
        assert!(
            error.to_string().contains("stream parsing limit"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[test]
    fn startup_skips_pdf_asset_that_exceeds_parser_limits() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes(
            "report.pdf",
            Some("application/pdf"),
            &sample_pdf_bytes("OLD PDF OK")?,
        )?;
        let bytes = pdf_with_extra_null_objects(MAX_PDF_OBJECTS + 1)?;
        let raw_path = asset_path_from_uri(&store.root, &asset.uri)?;
        write_file_atomic(&raw_path, &bytes)?;
        let mut record = asset.clone();
        let (sha256, byte_length) = payload_integrity(&bytes);
        record.sha256 = sha256;
        record.byte_length = byte_length;
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &record)?;

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(
            restarted.get(&asset.id).is_none(),
            "asset exceeding new PDF parser limits should be skipped"
        );
        let report = restarted.startup_repair_report();
        assert_eq!(report.skipped_asset_count, 1);
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.asset_id.as_deref() == Some(asset.id.as_str())
                && diagnostic.kind == "derived"
                && diagnostic.action == "skip_asset"
                && diagnostic.reason.contains("object parsing limit")
        }));
        Ok(())
    }

    #[test]
    fn dxf_import_writes_decodable_preview_png() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let dxf = store.import_bytes("plan.dxf", None, &sample_dxf_bytes())?;
        let preview_uri = dxf
            .preview_image_uri
            .as_deref()
            .ok_or_else(|| anyhow!("missing DXF preview uri"))?;
        let preview_path = asset_path_from_uri(&store.root, preview_uri)?;
        let preview_bytes = fs::read(&preview_path)?;
        let image = image::load_from_memory_with_format(&preview_bytes, ImageFormat::Png)
            .context("DXF preview was not a decodable PNG")?;
        assert!(image.width() >= 128);
        assert!(image.height() >= 128);
        Ok(())
    }

    #[test]
    fn dxf_preview_downscales_large_extents() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let dxf = store.import_bytes(
            "huge-plan.dxf",
            Some("application/dxf"),
            &dxf_with_large_extent_line(),
        )?;
        let preview_uri = dxf
            .preview_image_uri
            .as_deref()
            .ok_or_else(|| anyhow!("large extent DXF should still render a preview"))?;
        let preview_path = asset_path_from_uri(&store.root, preview_uri)?;
        let preview_bytes = fs::read(&preview_path)?;
        image::load_from_memory_with_format(&preview_bytes, ImageFormat::Png)
            .context("large extent DXF preview should be decodable")?;
        Ok(())
    }

    #[test]
    fn dxf_preview_omits_preview_after_segment_limit() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let dxf = store.import_bytes(
            "many-lines.dxf",
            Some("application/dxf"),
            &dxf_with_many_lines(MAX_DXF_PREVIEW_SEGMENTS + 1),
        )?;
        assert!(
            dxf.text_uri.is_some(),
            "DXF summary should still be derived when preview is omitted"
        );
        assert!(
            dxf.preview_image_uri.is_none(),
            "DXF preview should be omitted after segment budget is exceeded"
        );
        Ok(())
    }

    #[test]
    fn deduplicates_assets_by_media_type_and_digest() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let first = store.import_bytes("note.txt", Some("text/plain"), b"same")?;
        let second = store.import_bytes("copy.txt", Some("text/plain"), b"same")?;
        assert_eq!(first.id, second.id);
        Ok(())
    }

    #[test]
    fn read_raw_rejects_live_payload_integrity_mismatch() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let raw_path = asset_path_from_uri(&store.root, &asset.uri)?;
        write_file_atomic(&raw_path, b"tampered")?;

        let error = store
            .read_raw(&asset.id)
            .expect_err("live raw tampering should fail integrity validation");
        assert_asset_integrity_error(&error);
        Ok(())
    }

    #[test]
    fn import_writes_derived_integrity_metadata_and_attachment_refs() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let text = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        assert_eq!(
            text.text_sha256,
            Some(hex::encode(Sha256::digest(b"hello")))
        );
        assert_eq!(text.text_byte_length, Some(5));
        let text_ref = text.attachment_ref();
        assert_eq!(text_ref.text_sha256, text.text_sha256);
        assert_eq!(text_ref.text_byte_length, text.text_byte_length);

        let dxf = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        assert!(dxf.text_uri.is_some());
        assert!(dxf.text_sha256.is_some());
        assert!(dxf.text_byte_length.is_some());
        assert!(dxf.preview_image_uri.is_some());
        assert_eq!(dxf.preview_image_media_type.as_deref(), Some("image/png"));
        assert!(dxf.preview_image_sha256.is_some());
        assert!(dxf.preview_image_byte_length.is_some());
        let dxf_ref = dxf.attachment_ref();
        assert_eq!(dxf_ref.preview_image_sha256, dxf.preview_image_sha256);
        assert_eq!(
            dxf_ref.preview_image_byte_length,
            dxf.preview_image_byte_length
        );
        Ok(())
    }

    #[test]
    fn read_text_rejects_live_derived_text_integrity_mismatch() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let text_uri = asset
            .text_uri
            .as_deref()
            .ok_or_else(|| anyhow!("text asset should expose derived text"))?;
        let text_path = asset_path_from_uri(&store.root, text_uri)?;
        write_file_atomic(&text_path, b"HELLO")?;

        let read_error = store
            .read_text(&asset.id)
            .expect_err("live derived text tampering should fail integrity validation");
        assert_asset_integrity_error(&read_error);
        let render_error = store
            .render_asset_transcript_part(&asset)
            .expect_err("transcript rendering should not bypass derived text integrity");
        assert_asset_integrity_error(&render_error);
        Ok(())
    }

    #[test]
    fn read_preview_rejects_live_derived_preview_integrity_mismatch() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        let preview_uri = asset
            .preview_image_uri
            .as_deref()
            .ok_or_else(|| anyhow!("DXF asset should expose a preview image"))?;
        let preview_path = asset_path_from_uri(&store.root, preview_uri)?;
        write_file_atomic(&preview_path, &sample_png_bytes()?)?;

        let error = store
            .read_preview_image(&asset.id)
            .expect_err("live derived preview tampering should fail integrity validation");
        assert_asset_integrity_error(&error);
        Ok(())
    }

    #[test]
    fn delete_asset_does_not_delete_attached_text_asset_raw_payload() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let audio = store.import_bytes("call.wav", Some("audio/wav"), &sample_wav_bytes())?;
        let transcript =
            store.import_bytes("call.transcript.txt", Some("text/plain"), b"TRANSCRIPT_OK")?;
        store.attach_text_asset(&audio.id, &transcript)?;
        let transcript_raw_path = asset_path_from_uri(&store.root, &transcript.uri)?;

        assert!(store.delete_asset(&audio.id)?);
        assert!(store.get(&audio.id).is_none());
        assert!(store.get(&transcript.id).is_some());
        assert!(
            transcript_raw_path.is_file(),
            "deleting the audio asset must not remove the attached text asset raw payload"
        );
        let (_, bytes) = store.read_raw(&transcript.id)?;
        assert_eq!(bytes, b"TRANSCRIPT_OK");
        Ok(())
    }

    #[test]
    fn delete_asset_preserves_shared_transcript_for_remaining_audio_asset() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let first_audio =
            store.import_bytes("first.wav", Some("audio/wav"), &sample_wav_bytes())?;
        let second_audio =
            store.import_bytes("second.webm", Some("audio/webm"), &sample_webm_bytes())?;
        let transcript = store.import_bytes(
            "shared.transcript.txt",
            Some("text/plain"),
            b"SHARED_TEXT_OK",
        )?;
        store.attach_text_asset(&first_audio.id, &transcript)?;
        store.attach_text_asset(&second_audio.id, &transcript)?;

        assert!(store.delete_asset(&first_audio.id)?);
        let second_text = store
            .read_text(&second_audio.id)?
            .ok_or_else(|| anyhow!("second audio should still expose shared transcript"))?;
        assert_eq!(second_text, "SHARED_TEXT_OK");
        Ok(())
    }

    #[test]
    fn attach_text_asset_persists_target_integrity_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let audio = store.import_bytes("call.wav", Some("audio/wav"), &sample_wav_bytes())?;
        let transcript =
            store.import_bytes("call.transcript.txt", Some("text/plain"), b"TRANSCRIPT_OK")?;

        let updated = store.attach_text_asset(&audio.id, &transcript)?;
        assert_eq!(updated.text_uri.as_deref(), Some(transcript.uri.as_str()));
        assert_eq!(
            updated.text_sha256.as_deref(),
            Some(transcript.sha256.as_str())
        );
        assert_eq!(updated.text_byte_length, Some(transcript.byte_length));

        let transcript_raw_path = asset_path_from_uri(&store.root, &transcript.uri)?;
        write_file_atomic(&transcript_raw_path, b"TRANSCRIPT_BAD")?;
        let error = store
            .read_text(&audio.id)
            .expect_err("attached text tampering should fail parent integrity validation");
        assert_asset_integrity_error(&error);
        Ok(())
    }

    fn write_test_asset_tombstone(
        store: &FileAssetStore,
        asset: &StoredAssetRecord,
        reason: &str,
    ) -> Result<()> {
        write_asset_tombstone_atomic(
            &store.asset_tombstone_path(&asset.id),
            &AssetTombstoneRecord {
                asset_id: asset.id.clone(),
                media_type: asset.media_type.clone(),
                file_name: asset.file_name.clone(),
                sha256: asset.sha256.clone(),
                byte_length: asset.byte_length,
                deleted_at_ms: now_ms(),
                reason: reason.to_string(),
            },
        )
    }

    #[test]
    fn delete_asset_writes_tombstone_and_prevents_id_reuse_after_restart() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let first = store.import_bytes("first.txt", Some("text/plain"), b"first")?;
        assert_eq!(first.id, "asset-1");

        assert!(store.delete_asset_with_reason(&first.id, "retention")?);
        let tombstone_path = store.asset_tombstone_path(&first.id);
        assert!(tombstone_path.is_file());
        let tombstone: AssetTombstoneRecord = serde_json::from_slice(&fs::read(&tombstone_path)?)?;
        assert_eq!(tombstone.asset_id, first.id);
        assert_eq!(tombstone.reason, "retention");

        let restarted = FileAssetStore::new(temp.path())?;
        let second = restarted.import_bytes("second.txt", Some("text/plain"), b"second")?;
        assert_eq!(second.id, "asset-2");
        Ok(())
    }

    #[test]
    fn startup_completes_tombstoned_asset_delete_crash_window() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        assert_eq!(asset.id, "asset-1");
        let raw_path = asset_path_from_uri(&store.root, &asset.uri)?;
        let text_path = asset_path_from_uri(
            &store.root,
            asset
                .text_uri
                .as_deref()
                .ok_or_else(|| anyhow!("text asset should expose derived text"))?,
        )?;
        let preview_path = asset_path_from_uri(
            &store.root,
            asset
                .preview_image_uri
                .as_deref()
                .ok_or_else(|| anyhow!("DXF asset should expose derived preview"))?,
        )?;
        let meta_path = store.asset_meta_path(&asset.id);
        write_test_asset_tombstone(&store, &asset, "crash-window-test")?;
        fs::write(
            store.root.join("tombstones").join("asset-2.foo.json"),
            b"{}",
        )?;
        let raw_asset_10 = store.root.join("raw").join("asset-10.txt");
        let text_asset_10 = store.root.join("text").join("asset-10.txt");
        let preview_asset_10 = store.root.join("preview").join("asset-10.png");
        let raw_asset_1backup = store.root.join("raw").join("asset-1backup.txt");
        fs::write(&raw_asset_10, b"asset-10")?;
        fs::write(&text_asset_10, b"asset-10 text")?;
        fs::write(&preview_asset_10, sample_png_bytes()?)?;
        fs::write(&raw_asset_1backup, b"not daemon-owned")?;
        assert!(raw_path.is_file());
        assert!(text_path.is_file());
        assert!(preview_path.is_file());
        assert!(meta_path.is_file());
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(
            restarted.get(&asset.id).is_none(),
            "tombstoned asset must not be reloaded after restart"
        );
        let report = restarted.startup_repair_report();
        assert_eq!(report.completed_tombstone_delete_count, 1);
        assert!(report.repaired_count >= 1);
        assert!(!raw_path.exists());
        assert!(!text_path.exists());
        assert!(!preview_path.exists());
        assert!(!meta_path.exists());
        assert!(raw_asset_10.is_file());
        assert!(text_asset_10.is_file());
        assert!(preview_asset_10.is_file());
        assert!(raw_asset_1backup.is_file());
        let second = restarted.import_bytes("second.txt", Some("text/plain"), b"second")?;
        assert_eq!(
            second.id, "asset-2",
            "tombstoned id must remain reserved after crash-window cleanup"
        );
        Ok(())
    }

    #[test]
    fn startup_tombstone_cleanup_handles_corrupt_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset_id = "asset-1";
        let raw_path = store.root.join("raw").join("asset-1.bin");
        let text_path = store.root.join("text").join("asset-1.txt");
        let preview_path = store.root.join("preview").join("asset-1.png");
        let meta_path = store.asset_meta_path(asset_id);
        fs::write(&raw_path, b"raw")?;
        fs::write(&text_path, b"text")?;
        fs::write(&preview_path, sample_png_bytes()?)?;
        fs::write(&meta_path, b"{not valid json")?;
        write_asset_tombstone_atomic(
            &store.asset_tombstone_path(asset_id),
            &AssetTombstoneRecord {
                asset_id: asset_id.to_string(),
                media_type: "application/octet-stream".to_string(),
                file_name: "crashed.bin".to_string(),
                sha256: "unknown".to_string(),
                byte_length: 3,
                deleted_at_ms: now_ms(),
                reason: "corrupt-meta-crash".to_string(),
            },
        )?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(restarted.get(asset_id).is_none());
        assert_eq!(
            restarted
                .startup_repair_report()
                .completed_tombstone_delete_count,
            1
        );
        assert!(!raw_path.exists());
        assert!(!text_path.exists());
        assert!(!preview_path.exists());
        assert!(!meta_path.exists());
        let next = restarted.import_bytes("next.txt", Some("text/plain"), b"next")?;
        assert_eq!(next.id, "asset-2");
        Ok(())
    }

    #[test]
    fn startup_ignores_invalid_tombstone_without_deleting_asset() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let raw_path = asset_path_from_uri(&store.root, &asset.uri)?;
        let tombstone_path = store.asset_tombstone_path(&asset.id);
        fs::write(&tombstone_path, b"{not valid json")?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(
            restarted.get(&asset.id).is_some(),
            "invalid tombstone must not delete a valid asset"
        );
        assert_eq!(restarted.startup_repair_report().invalid_tombstone_count, 1);
        assert!(raw_path.is_file());
        assert!(!tombstone_path.exists());
        let next = restarted.import_bytes("next.txt", Some("text/plain"), b"next")?;
        assert_eq!(next.id, "asset-2");
        Ok(())
    }

    #[test]
    fn startup_tombstone_cleanup_keeps_attached_text_asset_payload() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let audio = store.import_bytes("call.wav", Some("audio/wav"), &sample_wav_bytes())?;
        let transcript =
            store.import_bytes("call.transcript.txt", Some("text/plain"), b"TRANSCRIPT_OK")?;
        let updated_audio = store.attach_text_asset(&audio.id, &transcript)?;
        let transcript_raw_path = asset_path_from_uri(&store.root, &transcript.uri)?;
        write_test_asset_tombstone(&store, &updated_audio, "audio-delete-crash")?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(restarted.get(&audio.id).is_none());
        assert!(
            restarted.get(&transcript.id).is_some(),
            "attached text asset must stay loadable after parent tombstone cleanup"
        );
        assert!(transcript_raw_path.is_file());
        let (_, bytes) = restarted.read_raw(&transcript.id)?;
        assert_eq!(bytes, b"TRANSCRIPT_OK");
        Ok(())
    }

    #[test]
    fn startup_tombstone_cleanup_ignores_cross_asset_raw_uri_in_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let first = store.import_bytes("first.txt", Some("text/plain"), b"first")?;
        let second = store.import_bytes("second.txt", Some("text/plain"), b"second")?;
        let second_raw_path = asset_path_from_uri(&store.root, &second.uri)?;
        let mut corrupted_first = first.clone();
        corrupted_first.uri = second.uri.clone();
        write_asset_metadata_atomic(&store.asset_meta_path(&first.id), &corrupted_first)?;
        write_test_asset_tombstone(&store, &first, "cross-asset-raw-crash")?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(restarted.get(&first.id).is_none());
        assert!(
            restarted.get(&second.id).is_some(),
            "tombstone cleanup must not delete a raw URI owned by another asset"
        );
        assert!(second_raw_path.is_file());
        Ok(())
    }

    #[test]
    fn startup_tombstone_cleanup_ignores_preview_subpath_uri_in_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        let nested_preview_dir = store.root.join("preview").join("asset-1.bad");
        fs::create_dir_all(&nested_preview_dir)?;
        let nested_preview_path = nested_preview_dir.join("evil");
        fs::write(&nested_preview_path, b"do-not-delete")?;
        let mut corrupted = asset.clone();
        corrupted.preview_image_uri = Some("asset://preview/asset-1.bad/evil".to_string());
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &corrupted)?;
        write_test_asset_tombstone(&store, &asset, "preview-subpath-crash")?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert!(restarted.get(&asset.id).is_none());
        assert!(
            nested_preview_path.is_file(),
            "preview subpath URI in metadata must not be treated as an owned daemon payload"
        );
        Ok(())
    }

    #[test]
    fn concurrent_imports_share_one_deduplicated_record() -> Result<()> {
        let temp = TempDir::new()?;
        let store = Arc::new(FileAssetStore::new(temp.path())?);
        let payload = sample_dxf_bytes();
        let mut workers = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let payload = payload.clone();
            workers.push(thread::spawn(move || {
                store.import_bytes("plan.dxf", Some("application/dxf"), &payload)
            }));
        }
        let imported = workers
            .into_iter()
            .map(|worker| worker.join().expect("asset import worker panicked"))
            .collect::<Result<Vec<_>>>()?;
        let first_id = imported
            .first()
            .map(|record| record.id.clone())
            .ok_or_else(|| anyhow!("missing concurrent import results"))?;
        assert!(imported.iter().all(|record| record.id == first_id));
        assert_eq!(store.list(None).len(), 1);
        Ok(())
    }

    #[test]
    fn startup_skips_metadata_when_raw_payload_is_missing() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let raw_path = asset_path_from_uri(&store.root, &asset.uri)?;
        fs::remove_file(raw_path)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        assert!(
            repaired.get(&asset.id).is_none(),
            "asset with missing raw payload should not be exposed after restart"
        );
        assert_eq!(repaired.startup_repair_report().skipped_asset_count, 1);
        assert_eq!(
            repaired.startup_repair_report().skipped_raw_missing_count,
            1
        );
        assert!(repaired.list(None).is_empty());
        let next = repaired.import_bytes("next.txt", Some("text/plain"), b"next")?;
        assert_eq!(
            next.id, "asset-2",
            "skipped metadata must still reserve its previous id suffix"
        );
        Ok(())
    }

    #[test]
    fn startup_skips_metadata_when_raw_payload_digest_mismatches() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let raw_path = asset_path_from_uri(&store.root, &asset.uri)?;
        write_file_atomic(&raw_path, b"tampered")?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        assert!(
            repaired.get(&asset.id).is_none(),
            "asset with tampered raw payload should not be exposed after restart"
        );
        assert_eq!(repaired.startup_repair_report().skipped_asset_count, 1);
        assert_eq!(
            repaired
                .startup_repair_report()
                .skipped_raw_integrity_mismatch_count,
            1
        );
        assert!(repaired.list(None).is_empty());
        let next = repaired.import_bytes("next.txt", Some("text/plain"), b"next")?;
        assert_eq!(
            next.id, "asset-2",
            "skipped metadata must still reserve its previous id suffix"
        );
        Ok(())
    }

    #[test]
    fn startup_skips_cross_asset_raw_uri_even_when_integrity_matches() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let first = store.import_bytes("first.txt", Some("text/plain"), b"first")?;
        let second = store.import_bytes("second.txt", Some("text/plain"), b"second")?;
        let second_raw_path = asset_path_from_uri(&store.root, &second.uri)?;
        let mut corrupted_first = first.clone();
        corrupted_first.uri = second.uri.clone();
        corrupted_first.sha256 = second.sha256.clone();
        corrupted_first.byte_length = second.byte_length;
        write_asset_metadata_atomic(&store.asset_meta_path(&first.id), &corrupted_first)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        assert!(
            repaired.get(&first.id).is_none(),
            "metadata whose raw URI belongs to another asset must not load"
        );
        assert!(repaired.get(&second.id).is_some());
        assert!(second_raw_path.is_file());
        assert_eq!(repaired.startup_repair_report().skipped_asset_count, 1);
        assert!(!repaired.delete_asset(&first.id)?);
        assert!(repaired.delete_asset(&second.id)?);
        assert!(
            !second_raw_path.exists(),
            "deleting the real owner should remove its own raw payload"
        );
        Ok(())
    }

    #[test]
    fn startup_quarantines_corrupt_asset_metadata_and_preserves_id_seed() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let meta_path = store.asset_meta_path(&asset.id);
        write_file_atomic(&meta_path, b"{ this is not valid json")?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        assert!(
            repaired.get(&asset.id).is_none(),
            "asset with corrupt metadata should not be exposed after restart"
        );
        assert!(
            has_file_name_with_prefix(
                &repaired.root.join("meta"),
                &format!("{}.json.corrupt-", asset.id),
            )?,
            "corrupt metadata should be quarantined instead of aborting startup"
        );
        let next = repaired.import_bytes("next.txt", Some("text/plain"), b"next")?;
        assert_eq!(
            next.id, "asset-2",
            "quarantined metadata must still reserve its previous id suffix"
        );
        drop(repaired);

        let restarted = FileAssetStore::new(temp.path())?;
        let following = restarted.import_bytes("later.txt", Some("text/plain"), b"later")?;
        assert_eq!(
            following.id, "asset-3",
            "quarantined metadata siblings should keep seeding future ids"
        );
        Ok(())
    }

    #[test]
    fn startup_skips_traversal_raw_storage_uri() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let mut record = asset.clone();
        record.uri = "asset://raw/../../outside.txt".to_string();
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &record)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        assert!(repaired.get(&asset.id).is_none());
        assert_eq!(repaired.startup_repair_report().skipped_asset_count, 1);
        assert!(
            repaired
                .startup_repair_report()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason == "raw_uri_not_owned_by_asset"),
            "startup should report the invalid raw URI"
        );
        Ok(())
    }

    #[test]
    fn startup_restores_missing_derived_asset_references() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let text_uri = asset
            .text_uri
            .as_deref()
            .ok_or_else(|| anyhow!("text asset should expose derived text"))?;
        let text_path = asset_path_from_uri(&store.root, text_uri)?;
        fs::remove_file(&text_path)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should still load after derived reference repair"))?;
        assert_eq!(loaded.text_uri.as_deref(), Some(text_uri));
        assert_eq!(
            loaded.text_sha256,
            Some(hex::encode(Sha256::digest(b"hello")))
        );
        assert_eq!(loaded.text_byte_length, Some(5));
        assert_eq!(repaired.read_text(&asset.id)?.as_deref(), Some("hello"));
        assert!(text_path.is_file());
        let report = repaired.startup_repair_report();
        assert_eq!(report.restored_derived_text_count, 1);
        assert_eq!(report.repaired_count, 1);
        Ok(())
    }

    #[test]
    fn startup_restores_invalid_derived_text_uri() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let invalid_text_uri = "asset://text/asset-1.bad";
        write_file_atomic(&store.root.join("text").join("asset-1.bad"), b"hello")?;
        let mut record = asset.clone();
        record.text_uri = Some(invalid_text_uri.to_string());
        record.text_sha256 = None;
        record.text_byte_length = None;
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &record)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should load after invalid text URI repair"))?;
        assert_eq!(loaded.text_uri.as_deref(), Some("asset://text/asset-1.txt"));
        assert_eq!(repaired.read_text(&asset.id)?.as_deref(), Some("hello"));
        assert!(
            repaired
                .startup_repair_report()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason == "derived_text_uri_not_owned")
        );
        Ok(())
    }

    #[test]
    fn startup_preserves_verified_attached_text_asset_uri() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let audio = store.import_bytes("call.wav", Some("audio/wav"), &sample_wav_bytes())?;
        let transcript =
            store.import_bytes("call.transcript.txt", Some("text/plain"), b"TRANSCRIPT_OK")?;
        let updated = store.attach_text_asset(&audio.id, &transcript)?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        let loaded = restarted
            .get(&audio.id)
            .ok_or_else(|| anyhow!("audio asset should load after restart"))?;
        assert_eq!(loaded.text_uri.as_deref(), Some(transcript.uri.as_str()));
        assert_eq!(
            restarted.read_text(&audio.id)?.as_deref(),
            Some("TRANSCRIPT_OK")
        );
        assert_eq!(restarted.startup_repair_report().repaired_count, 0);
        assert_eq!(updated.text_uri, loaded.text_uri);
        Ok(())
    }

    #[test]
    fn startup_backfills_legacy_derived_integrity_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let mut record = asset.clone();
        record.text_sha256 = None;
        record.text_byte_length = None;
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &record)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should still load after legacy metadata backfill"))?;
        assert_eq!(
            loaded.text_sha256,
            Some(hex::encode(Sha256::digest(b"hello")))
        );
        assert_eq!(loaded.text_byte_length, Some(5));

        let persisted: StoredAssetRecord =
            serde_json::from_slice(&fs::read(repaired.asset_meta_path(&asset.id))?)?;
        assert_eq!(persisted.text_sha256, loaded.text_sha256);
        assert_eq!(persisted.text_byte_length, loaded.text_byte_length);
        assert_eq!(
            repaired.startup_repair_report().integrity_backfilled_count,
            1
        );
        Ok(())
    }

    #[test]
    fn clean_startup_reports_no_asset_repairs_for_image_and_audio() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let image = store.import_bytes("pixel.png", Some("image/png"), &sample_png_bytes()?)?;
        let audio = store.import_bytes("call.wav", Some("audio/wav"), &sample_wav_bytes())?;
        drop(store);

        let restarted = FileAssetStore::new(temp.path())?;
        assert_eq!(
            restarted.startup_repair_report(),
            AssetStartupRepairReport::default()
        );
        let image = restarted
            .get(&image.id)
            .ok_or_else(|| anyhow!("image should survive clean restart"))?;
        let audio = restarted
            .get(&audio.id)
            .ok_or_else(|| anyhow!("audio should survive clean restart"))?;
        assert!(image.text_uri.is_none());
        assert!(image.preview_image_uri.is_none());
        assert!(audio.text_uri.is_none());
        assert!(audio.preview_image_uri.is_none());
        Ok(())
    }

    #[test]
    fn startup_restores_tampered_derived_text_reference() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let text_uri = asset
            .text_uri
            .as_deref()
            .ok_or_else(|| anyhow!("text asset should expose derived text"))?;
        let text_path = asset_path_from_uri(&store.root, text_uri)?;
        write_file_atomic(&text_path, b"HELLO")?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should still load after text metadata repair"))?;
        assert_eq!(loaded.text_uri.as_deref(), Some(text_uri));
        assert_eq!(
            loaded.text_sha256,
            Some(hex::encode(Sha256::digest(b"hello")))
        );
        assert_eq!(loaded.text_byte_length, Some(5));
        assert_eq!(repaired.read_text(&asset.id)?.as_deref(), Some("hello"));
        assert_eq!(
            repaired.startup_repair_report().restored_derived_text_count,
            1
        );
        Ok(())
    }

    #[test]
    fn startup_restores_preview_when_media_type_is_missing() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        assert!(asset.preview_image_uri.is_some());
        assert_eq!(asset.preview_image_media_type.as_deref(), Some("image/png"));
        let mut record = asset.clone();
        record.preview_image_media_type = None;
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &record)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should still load after preview metadata repair"))?;
        assert_eq!(
            loaded.preview_image_uri.as_deref(),
            asset.preview_image_uri.as_deref()
        );
        assert_eq!(
            loaded.preview_image_media_type.as_deref(),
            Some("image/png")
        );
        assert!(loaded.preview_image_sha256.is_some());
        assert!(loaded.preview_image_byte_length.is_some());
        let (preview_media_type, preview_bytes) = repaired
            .read_preview_image(&asset.id)?
            .ok_or_else(|| anyhow!("preview should be restored"))?;
        assert_eq!(preview_media_type, "image/png");
        image::load_from_memory_with_format(&preview_bytes, ImageFormat::Png)
            .context("restored preview should be a decodable PNG")?;
        assert_eq!(
            repaired
                .startup_repair_report()
                .restored_derived_preview_count,
            1
        );
        Ok(())
    }

    #[test]
    fn startup_restores_invalid_derived_preview_uri_without_aborting() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        let mut record = asset.clone();
        record.preview_image_uri = Some("asset://preview/../../outside.png".to_string());
        write_asset_metadata_atomic(&store.asset_meta_path(&asset.id), &record)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should load after invalid preview URI repair"))?;
        assert_eq!(
            loaded.preview_image_uri.as_deref(),
            Some("asset://preview/asset-1.png")
        );
        assert!(repaired.read_preview_image(&asset.id)?.is_some());
        assert!(
            repaired
                .startup_repair_report()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason == "derived_preview_uri_invalid")
        );
        Ok(())
    }

    #[test]
    fn startup_restores_dxf_text_and_preview_in_one_pass() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        let text_uri = asset
            .text_uri
            .as_deref()
            .ok_or_else(|| anyhow!("DXF asset should expose derived text"))?;
        let preview_uri = asset
            .preview_image_uri
            .as_deref()
            .ok_or_else(|| anyhow!("DXF asset should expose derived preview"))?;
        let text_path = asset_path_from_uri(&store.root, text_uri)?;
        let preview_path = asset_path_from_uri(&store.root, preview_uri)?;
        fs::remove_file(&text_path)?;
        write_file_atomic(&preview_path, &sample_png_bytes()?)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("DXF asset should still load after derived repair"))?;
        assert_eq!(loaded.text_uri.as_deref(), Some(text_uri));
        assert_eq!(loaded.preview_image_uri.as_deref(), Some(preview_uri));
        assert!(text_path.is_file());
        assert!(preview_path.is_file());
        let report = repaired.startup_repair_report();
        assert_eq!(report.restored_derived_text_count, 1);
        assert_eq!(report.restored_derived_preview_count, 1);
        Ok(())
    }

    #[test]
    fn startup_restores_missing_pdf_derived_text() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes(
            "report.pdf",
            Some("application/pdf"),
            &sample_pdf_bytes("PDF REPAIR OK")?,
        )?;
        let text_uri = asset
            .text_uri
            .as_deref()
            .ok_or_else(|| anyhow!("PDF asset should expose derived text"))?;
        let text_path = asset_path_from_uri(&store.root, text_uri)?;
        fs::remove_file(&text_path)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let text = repaired
            .read_text(&asset.id)?
            .ok_or_else(|| anyhow!("PDF text should be restored"))?;
        assert!(text.contains("PDF REPAIR OK"));
        assert!(text_path.is_file());
        assert_eq!(
            repaired.startup_repair_report().restored_derived_text_count,
            1
        );
        Ok(())
    }

    #[test]
    fn startup_restores_tampered_derived_preview_reference() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("plan.dxf", Some("application/dxf"), &sample_dxf_bytes())?;
        let preview_uri = asset
            .preview_image_uri
            .as_deref()
            .ok_or_else(|| anyhow!("DXF asset should expose a preview image"))?;
        let preview_path = asset_path_from_uri(&store.root, preview_uri)?;
        write_file_atomic(&preview_path, &sample_png_bytes()?)?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        let loaded = repaired
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should still load after preview metadata repair"))?;
        assert_eq!(loaded.preview_image_uri.as_deref(), Some(preview_uri));
        assert_eq!(
            loaded.preview_image_media_type.as_deref(),
            Some("image/png")
        );
        assert_eq!(loaded.preview_image_sha256, asset.preview_image_sha256);
        assert_eq!(
            loaded.preview_image_byte_length,
            asset.preview_image_byte_length
        );
        let (preview_media_type, preview_bytes) = repaired
            .read_preview_image(&asset.id)?
            .ok_or_else(|| anyhow!("preview should be restored"))?;
        assert_eq!(preview_media_type, "image/png");
        image::load_from_memory_with_format(&preview_bytes, ImageFormat::Png)
            .context("restored preview should be a decodable PNG")?;
        assert_eq!(
            repaired
                .startup_repair_report()
                .restored_derived_preview_count,
            1
        );
        Ok(())
    }

    #[test]
    fn attach_derivation_rolls_back_catalog_when_metadata_write_fails() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let meta_path = store.asset_meta_path(&asset.id);
        fs::remove_file(&meta_path)?;
        fs::create_dir(&meta_path)?;

        let error = store
            .attach_derivation(&asset.id, "derivation-1")
            .expect_err("metadata directory should make attach_derivation fail");
        assert!(
            error.to_string().contains("failed to write asset metadata"),
            "unexpected error: {error:#}"
        );
        let loaded = store
            .get(&asset.id)
            .ok_or_else(|| anyhow!("asset should remain in catalog after rollback"))?;
        assert!(loaded.derivation_ids.is_empty());
        Ok(())
    }

    #[test]
    fn attach_text_asset_rolls_back_catalog_when_metadata_write_fails() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let audio = store.import_bytes("call.wav", Some("audio/wav"), &sample_wav_bytes())?;
        let transcript =
            store.import_bytes("call.transcript.txt", Some("text/plain"), b"TRANSCRIPT_OK")?;
        let meta_path = store.asset_meta_path(&audio.id);
        fs::remove_file(&meta_path)?;
        fs::create_dir(&meta_path)?;

        let error = store
            .attach_text_asset(&audio.id, &transcript)
            .expect_err("metadata directory should make attach_text_asset fail");
        assert!(
            error.to_string().contains("failed to write asset metadata"),
            "unexpected error: {error:#}"
        );
        let loaded = store
            .get(&audio.id)
            .ok_or_else(|| anyhow!("audio asset should remain in catalog after rollback"))?;
        assert!(loaded.text_uri.is_none());
        Ok(())
    }

    #[test]
    fn startup_removes_asset_atomic_temp_files_without_deleting_non_temp_orphans() -> Result<()> {
        let temp = TempDir::new()?;
        let store = FileAssetStore::new(temp.path())?;
        let asset = store.import_bytes("note.txt", Some("text/plain"), b"hello")?;
        let root = store.root.clone();
        let temp_paths = [
            root.join("meta").join(".asset-999.json.tmp-123-1"),
            root.join("raw").join(".asset-999.bin.tmp-123-2"),
            root.join("text").join(".asset-999.txt.tmp-123-3"),
            root.join("preview").join(".asset-999.png.tmp-123-4"),
        ];
        for path in &temp_paths {
            fs::write(path, b"partial")?;
        }
        let raw_orphan = root.join("raw").join("asset-999.bin");
        let non_asset_temp = root.join("raw").join(".note.tmp-123-5");
        let malformed_asset_temp = root.join("raw").join(".asset-999.bin.tmp-pid-6");
        fs::write(&raw_orphan, b"orphan")?;
        fs::write(&non_asset_temp, b"not an asset temp")?;
        fs::write(&malformed_asset_temp, b"not an atomic temp suffix")?;
        drop(store);

        let repaired = FileAssetStore::new(temp.path())?;
        assert!(repaired.get(&asset.id).is_some());
        for path in &temp_paths {
            assert!(
                !path.exists(),
                "asset atomic temp file should be cleaned: {}",
                path.display()
            );
        }
        assert!(raw_orphan.is_file(), "real orphans are not GC'd by startup");
        assert!(
            non_asset_temp.is_file(),
            "non-asset hidden files are left alone"
        );
        assert!(
            malformed_asset_temp.is_file(),
            "malformed asset temp names are left alone"
        );
        Ok(())
    }

    #[test]
    fn rejects_plain_text_payloads_renamed_as_dxf() {
        let temp = TempDir::new().expect("temp dir");
        let store = FileAssetStore::new(temp.path()).expect("asset store");
        let error = store
            .import_bytes("not-a-plan.dxf", Some("application/dxf"), b"hello world")
            .expect_err("plain text should not pass DXF validation");
        assert!(
            error
                .to_string()
                .contains("recognizable DXF structure markers"),
            "unexpected error: {error:#}"
        );
    }
}
