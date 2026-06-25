//! Shared attachment helpers used by provider adapters.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kheish_types::{
    AttachmentRef, DEFAULT_DOCUMENT_ATTACHMENT_TEXT_CHAR_LIMIT, InputContentPart,
    parse_asset_storage_uri, render_document_attachment_text,
};
use sha2::{Digest, Sha256};

const MAX_ATTACHMENT_CACHE_ENTRIES: usize = 256;
const MAX_ATTACHMENT_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Prepared image attachment data ready for provider-specific serialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedImageAttachment {
    /// The stable daemon-owned asset identifier.
    pub id: String,
    /// The normalized MIME type accepted by the provider.
    pub media_type: String,
    /// The original file name when one is available.
    pub file_name: Option<String>,
    /// The base64-encoded raw payload.
    pub base64_data: String,
    /// The raw byte length used for provider-specific preflight checks.
    pub size_bytes: usize,
}

impl PreparedImageAttachment {
    /// Returns a data URL representation for providers that accept inline URLs.
    pub(crate) fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.base64_data)
    }
}

/// Small bounded in-process cache for prepared attachment payloads.
#[derive(Clone, Default)]
pub(crate) struct AttachmentRenderCache {
    inner: Arc<Mutex<AttachmentRenderCacheState>>,
}

#[derive(Default)]
struct AttachmentRenderCacheState {
    order: VecDeque<String>,
    values: HashMap<String, CachedAttachmentEntry>,
    total_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedAttachmentEntry {
    value: CachedAttachmentValue,
    size_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CachedAttachmentValue {
    Image(PreparedImageAttachment),
    DocumentText(String),
}

impl AttachmentRenderCache {
    /// Returns one cached image payload when present.
    pub(crate) fn get_image(&self, key: &str) -> Option<PreparedImageAttachment> {
        self.inner
            .lock()
            .expect("attachment render cache mutex poisoned")
            .values
            .get(key)
            .and_then(|entry| match &entry.value {
                CachedAttachmentValue::Image(image) => Some(image.clone()),
                CachedAttachmentValue::DocumentText(_) => None,
            })
    }

    /// Returns one cached document text block when present.
    pub(crate) fn get_document_text(&self, key: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("attachment render cache mutex poisoned")
            .values
            .get(key)
            .and_then(|entry| match &entry.value {
                CachedAttachmentValue::Image(_) => None,
                CachedAttachmentValue::DocumentText(text) => Some(text.clone()),
            })
    }

    /// Caches one prepared image payload.
    pub(crate) fn put_image(&self, key: String, image: PreparedImageAttachment) {
        self.insert(key, CachedAttachmentValue::Image(image));
    }

    /// Caches one rendered document text block.
    pub(crate) fn put_document_text(&self, key: String, text: String) {
        self.insert(key, CachedAttachmentValue::DocumentText(text));
    }

    fn insert(&self, key: String, value: CachedAttachmentValue) {
        let mut state = self
            .inner
            .lock()
            .expect("attachment render cache mutex poisoned");
        let size_bytes = cached_value_size_bytes(&value);
        if !state.values.contains_key(&key) {
            state.order.push_back(key.clone());
        }
        if let Some(previous) = state
            .values
            .insert(key, CachedAttachmentEntry { value, size_bytes })
        {
            state.total_bytes = state.total_bytes.saturating_sub(previous.size_bytes);
        }
        state.total_bytes = state.total_bytes.saturating_add(size_bytes);
        while state.order.len() > MAX_ATTACHMENT_CACHE_ENTRIES
            || state.total_bytes > MAX_ATTACHMENT_CACHE_BYTES
        {
            if let Some(stale_key) = state.order.pop_front() {
                if let Some(stale_entry) = state.values.remove(&stale_key) {
                    state.total_bytes = state.total_bytes.saturating_sub(stale_entry.size_bytes);
                }
            }
        }
    }
}

fn cached_value_size_bytes(value: &CachedAttachmentValue) -> usize {
    match value {
        CachedAttachmentValue::Image(image) => {
            image.id.len()
                + image.media_type.len()
                + image.file_name.as_ref().map_or(0, String::len)
                + image.base64_data.len()
                + std::mem::size_of_val(&image.size_bytes)
        }
        CachedAttachmentValue::DocumentText(text) => text.len(),
    }
}

/// Returns whether the input sequence contains at least one true image attachment.
pub(crate) fn contains_supported_image_attachment(
    content_parts: &[InputContentPart],
    attachments: &[AttachmentRef],
) -> bool {
    if !content_parts.is_empty() {
        return content_parts.iter().any(|part| match part {
            InputContentPart::Text { .. } => false,
            InputContentPart::Attachment { attachment } => {
                is_supported_image_media_type(&attachment.media_type)
            }
        });
    }
    attachments
        .iter()
        .any(|attachment| is_supported_image_media_type(&attachment.media_type))
}

/// Returns one compact provider-facing hint that maps the next image attachment to its daemon ID.
///
/// This text is only meant for model context. It must not be reused for user-facing transcripts.
pub(crate) fn image_edit_attachment_hint_text(attachment: &AttachmentRef) -> Option<String> {
    if !is_supported_image_media_type(&attachment.media_type) {
        return None;
    }
    let file_name = attachment
        .file_name
        .as_deref()
        .unwrap_or(attachment.id.as_str());
    Some(format!(
        "The next image attachment is daemon asset ID {} ({file_name}). If you call edit_image for this exact image, use image_asset_ids:[\"{}\"] and preserve the attachment order.",
        attachment.id, attachment.id
    ))
}

/// Loads one daemon-managed image attachment when the media type is image-compatible.
pub(crate) fn load_image_attachment(
    attachment: &AttachmentRef,
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> Result<Option<PreparedImageAttachment>> {
    if !is_supported_image_media_type(&attachment.media_type) {
        return Ok(None);
    }
    load_prepared_image_attachment(
        &attachment.id,
        &attachment.media_type,
        attachment.file_name.clone(),
        &attachment.uri,
        attachment.sha256.as_deref(),
        attachment.byte_length,
        image_cache_key(attachment),
        asset_root,
        cache,
    )
}

/// Loads one daemon-managed visual preview for a non-image attachment when available.
pub(crate) fn load_attachment_preview_image(
    attachment: &AttachmentRef,
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> Result<Option<PreparedImageAttachment>> {
    let Some(preview_uri) = attachment.preview_image_uri.as_ref() else {
        return Ok(None);
    };
    let Some(preview_media_type) = attachment.preview_image_media_type.as_ref() else {
        return Ok(None);
    };
    if !is_supported_image_media_type(preview_media_type) {
        return Ok(None);
    }
    let file_name = attachment.file_name.as_ref().map(|file_name| {
        let stem = Path::new(file_name)
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(file_name);
        match preview_media_type.as_str() {
            "image/png" => format!("{stem}.preview.png"),
            "image/jpeg" => format!("{stem}.preview.jpg"),
            _ => format!("{stem}.preview"),
        }
    });
    load_prepared_image_attachment(
        &format!("{}:preview", attachment.id),
        preview_media_type,
        file_name,
        preview_uri,
        attachment.preview_image_sha256.as_deref(),
        attachment.preview_image_byte_length,
        preview_cache_key(attachment),
        asset_root,
        cache,
    )
}

/// Loads one daemon-managed document attachment and returns the bounded text block shown to the model.
pub(crate) fn load_document_attachment_text(
    attachment: &AttachmentRef,
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> Result<Option<String>> {
    if is_supported_image_media_type(&attachment.media_type) {
        return Ok(None);
    }
    let Some(text_uri) = attachment.text_uri.as_ref() else {
        return Ok(None);
    };
    let cache_key = document_cache_key(attachment);
    let has_integrity_metadata =
        attachment.text_sha256.is_some() || attachment.text_byte_length.is_some();
    if !has_integrity_metadata && let Some(text) = cache.get_document_text(&cache_key) {
        return Ok(Some(text));
    }
    let text_path = resolve_attachment_path(
        &AttachmentRef {
            uri: text_uri.clone(),
            ..attachment.clone()
        },
        asset_root,
        // The derived text path reuses the same cache key as the source attachment.
    )?;
    let raw_bytes = fs::read(&text_path).with_context(|| {
        format!(
            "failed to read derived attachment text '{}' from {}",
            attachment.id,
            text_path.display()
        )
    })?;
    validate_attachment_payload_integrity(
        &attachment.id,
        "derived text",
        attachment.text_sha256.as_deref(),
        attachment.text_byte_length,
        &raw_bytes,
    )?;
    if let Some(text) = cache.get_document_text(&cache_key) {
        return Ok(Some(text));
    }
    let raw = String::from_utf8(raw_bytes)
        .with_context(|| format!("derived attachment text '{}' is not UTF-8", attachment.id))?;
    let rendered = render_document_attachment_text(
        attachment
            .file_name
            .as_deref()
            .unwrap_or(attachment.id.as_str()),
        &attachment.media_type,
        &raw,
        DEFAULT_DOCUMENT_ATTACHMENT_TEXT_CHAR_LIMIT,
    );
    cache.put_document_text(cache_key, rendered.clone());
    Ok(Some(rendered))
}

fn resolve_attachment_path(
    attachment: &AttachmentRef,
    asset_root: Option<&Path>,
) -> Result<PathBuf> {
    if let Some((kind, relative_path)) = parse_asset_storage_uri(&attachment.uri) {
        let root = asset_root.ok_or_else(|| {
            anyhow::anyhow!(
                "attachment '{}' requires a daemon asset root to resolve opaque storage URIs",
                attachment.id
            )
        })?;
        let base = match kind {
            "raw" => root.join("raw"),
            "text" => root.join("text"),
            "preview" => root.join("preview"),
            other => bail!("unsupported attachment storage kind '{other}'"),
        };
        validate_asset_relative_path(relative_path)?;
        return Ok(base.join(relative_path));
    }
    if asset_root.is_some() {
        bail!(
            "attachment '{}' uses unsupported non-daemon uri '{}'",
            attachment.id,
            attachment.uri
        );
    }
    Ok(PathBuf::from(&attachment.uri))
}

fn validate_asset_relative_path(relative_path: &str) -> Result<()> {
    let path = Path::new(relative_path);
    anyhow::ensure!(
        !path.is_absolute(),
        "attachment storage URI path must be relative"
    );
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                anyhow::ensure!(
                    !part.is_empty() && part != "." && part != "..",
                    "attachment storage URI path contains an invalid component"
                );
            }
            _ => bail!("attachment storage URI path contains an invalid component"),
        }
    }
    Ok(())
}

fn load_prepared_image_attachment(
    id: &str,
    media_type: &str,
    file_name: Option<String>,
    uri: &str,
    expected_sha256: Option<&str>,
    expected_byte_length: Option<u64>,
    cache_key: String,
    asset_root: Option<&Path>,
    cache: &AttachmentRenderCache,
) -> Result<Option<PreparedImageAttachment>> {
    let has_integrity_metadata = expected_sha256.is_some() || expected_byte_length.is_some();
    if !has_integrity_metadata && let Some(image) = cache.get_image(&cache_key) {
        return Ok(Some(image));
    }
    let asset_path = resolve_attachment_path(
        &AttachmentRef {
            id: id.to_string(),
            media_type: media_type.to_string(),
            uri: uri.to_string(),
            file_name: file_name.clone(),
            sha256: None,
            byte_length: None,
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        },
        asset_root,
    )?;
    let bytes = fs::read(&asset_path).with_context(|| {
        format!(
            "failed to read attachment '{id}' from {}",
            asset_path.display()
        )
    })?;
    if bytes.is_empty() {
        bail!("attachment '{id}' is empty");
    }
    validate_attachment_payload_integrity(
        id,
        "payload",
        expected_sha256,
        expected_byte_length,
        &bytes,
    )?;
    if let Some(image) = cache.get_image(&cache_key) {
        return Ok(Some(image));
    }
    let prepared = PreparedImageAttachment {
        id: id.to_string(),
        media_type: media_type.to_string(),
        file_name,
        base64_data: STANDARD.encode(&bytes),
        size_bytes: bytes.len(),
    };
    cache.put_image(cache_key, prepared.clone());
    Ok(Some(prepared))
}

/// Returns whether the MIME type is currently supported as a true multimodal image.
pub(crate) fn is_supported_image_media_type(media_type: &str) -> bool {
    matches!(
        media_type.trim().to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg"
    )
}

fn image_cache_key(attachment: &AttachmentRef) -> String {
    format!(
        "image:{}:{}:{}",
        attachment.media_type,
        attachment
            .sha256
            .as_deref()
            .unwrap_or(attachment.id.as_str()),
        attachment.uri
    )
}

fn preview_cache_key(attachment: &AttachmentRef) -> String {
    format!(
        "preview:{}:{}:{}:{}",
        attachment.id,
        attachment
            .preview_image_media_type
            .as_deref()
            .unwrap_or_default(),
        attachment.preview_image_uri.as_deref().unwrap_or_default(),
        attachment
            .preview_image_sha256
            .as_deref()
            .unwrap_or_default(),
    )
}

fn document_cache_key(attachment: &AttachmentRef) -> String {
    format!(
        "document:{}:{}:{}:{}",
        attachment.media_type,
        attachment
            .text_sha256
            .as_deref()
            .or(attachment.sha256.as_deref())
            .unwrap_or(attachment.id.as_str()),
        attachment
            .text_uri
            .as_deref()
            .unwrap_or(attachment.uri.as_str()),
        attachment.text_byte_length.unwrap_or_default()
    )
}

fn validate_attachment_payload_integrity(
    attachment_id: &str,
    label: &str,
    expected_sha256: Option<&str>,
    expected_byte_length: Option<u64>,
    bytes: &[u8],
) -> Result<()> {
    let actual_sha256 = hex::encode(Sha256::digest(bytes));
    let actual_byte_length = bytes.len() as u64;
    match (expected_sha256, expected_byte_length) {
        (Some(expected_sha256), Some(expected_byte_length)) => {
            anyhow::ensure!(
                actual_sha256 == expected_sha256 && actual_byte_length == expected_byte_length,
                "attachment integrity mismatch for {attachment_id}: {label} expected sha256 {expected_sha256} and {expected_byte_length} bytes, found sha256 {actual_sha256} and {actual_byte_length} bytes"
            );
        }
        (None, None) => {}
        _ => bail!(
            "attachment integrity mismatch for {attachment_id}: {label} metadata is incomplete"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_fixtures::create_fixture_dir;

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn daemon_asset_root_rejects_non_daemon_attachment_uris() {
        let temp = create_fixture_dir("attachment-cache-uris").expect("fixture dir");
        let attachment = AttachmentRef {
            id: "asset-1".to_string(),
            media_type: "text/plain".to_string(),
            uri: "/tmp/host-path.txt".to_string(),
            file_name: Some("host-path.txt".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some("/tmp/host-path.txt".to_string()),
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        let error = load_document_attachment_text(
            &attachment,
            Some(temp.as_path()),
            &AttachmentRenderCache::default(),
        )
        .expect_err("non-daemon uri should be rejected");
        assert!(
            error.to_string().contains("unsupported non-daemon uri"),
            "{error:#}"
        );
    }

    #[test]
    fn daemon_asset_root_rejects_traversal_storage_uris() {
        let temp = create_fixture_dir("attachment-cache-traversal").expect("fixture dir");
        let attachment = AttachmentRef {
            id: "asset-escape".to_string(),
            media_type: "text/plain".to_string(),
            uri: "asset://raw/../../outside.txt".to_string(),
            file_name: Some("outside.txt".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some("asset://text/../../outside.txt".to_string()),
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        let error = load_document_attachment_text(
            &attachment,
            Some(temp.as_path()),
            &AttachmentRenderCache::default(),
        )
        .expect_err("traversal uri should be rejected");
        assert!(error.to_string().contains("invalid component"), "{error:#}");
    }

    #[test]
    fn cache_evicts_entries_when_total_bytes_exceeds_budget() {
        let cache = AttachmentRenderCache::default();
        let large = "A".repeat((MAX_ATTACHMENT_CACHE_BYTES / 3) + 1024);
        cache.put_document_text("doc-1".to_string(), large.clone());
        cache.put_document_text("doc-2".to_string(), large.clone());
        cache.put_document_text("doc-3".to_string(), large);

        assert!(cache.get_document_text("doc-1").is_none());
        assert!(cache.get_document_text("doc-2").is_some());
        assert!(cache.get_document_text("doc-3").is_some());
    }

    #[test]
    fn detects_image_attachments_without_blocking_documents() {
        let document = AttachmentRef {
            id: "doc-1".to_string(),
            media_type: "application/pdf".to_string(),
            uri: "asset://raw/doc-1".to_string(),
            file_name: Some("doc.pdf".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some("asset://text/doc-1".to_string()),
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        let image = AttachmentRef {
            id: "img-1".to_string(),
            media_type: "image/png".to_string(),
            uri: "asset://raw/img-1".to_string(),
            file_name: Some("img.png".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };

        assert!(!contains_supported_image_attachment(
            &[],
            std::slice::from_ref(&document)
        ));
        assert!(contains_supported_image_attachment(
            &[],
            std::slice::from_ref(&image)
        ));
        assert!(contains_supported_image_attachment(
            &[InputContentPart::Attachment { attachment: image }],
            &[]
        ));
        assert!(!contains_supported_image_attachment(
            &[InputContentPart::Attachment {
                attachment: document,
            }],
            &[]
        ));
    }

    #[test]
    fn loads_document_preview_image_without_reclassifying_document_attachment() {
        let root = create_fixture_dir("attachment-preview").expect("fixture dir");
        let raw_path = root.join("plan.dxf");
        let text_path = root.join("plan.txt");
        let preview_path = root.join("plan.preview.png");
        fs::write(&raw_path, b"0\nEOF\n").expect("write raw fixture");
        fs::write(&text_path, b"DXF summary").expect("write text fixture");
        fs::write(&preview_path, crate::providers::test_fixtures::MINIMAL_PNG)
            .expect("write preview fixture");
        let attachment = AttachmentRef {
            id: "doc-preview".to_string(),
            media_type: "application/dxf".to_string(),
            uri: raw_path.display().to_string(),
            file_name: Some("plan.dxf".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some(text_path.display().to_string()),
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: Some(preview_path.display().to_string()),
            preview_image_media_type: Some("image/png".to_string()),
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };

        assert!(!contains_supported_image_attachment(
            &[],
            std::slice::from_ref(&attachment)
        ));
        let preview =
            load_attachment_preview_image(&attachment, None, &AttachmentRenderCache::default())
                .expect("preview should load")
                .expect("preview should exist");
        assert_eq!(preview.media_type, "image/png");
        assert_eq!(preview.file_name.as_deref(), Some("plan.preview.png"));
    }

    #[test]
    fn cached_raw_image_still_revalidates_payload_integrity() {
        let root = create_fixture_dir("attachment-raw-integrity-cache").expect("fixture dir");
        let raw_path = root.join("image.png");
        let raw_bytes = crate::providers::test_fixtures::MINIMAL_PNG;
        fs::write(&raw_path, raw_bytes).expect("write raw fixture");
        let attachment = AttachmentRef {
            id: "img-verified".to_string(),
            media_type: "image/png".to_string(),
            uri: raw_path.display().to_string(),
            file_name: Some("image.png".to_string()),
            sha256: Some(sha256_hex(raw_bytes)),
            byte_length: Some(raw_bytes.len() as u64),
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        let cache = AttachmentRenderCache::default();
        assert!(
            load_image_attachment(&attachment, None, &cache)
                .expect("initial image should load")
                .is_some()
        );
        fs::write(&raw_path, b"tampered").expect("tamper raw fixture");

        let error = load_image_attachment(&attachment, None, &cache)
            .expect_err("hot cache should not hide raw integrity mismatch");
        assert!(
            error.to_string().contains("attachment integrity mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn cached_document_text_still_revalidates_derived_integrity() {
        let root = create_fixture_dir("attachment-text-integrity-cache").expect("fixture dir");
        let raw_path = root.join("doc.txt");
        let text_path = root.join("doc.derived.txt");
        fs::write(&raw_path, b"raw").expect("write raw fixture");
        fs::write(&text_path, b"derived ok").expect("write text fixture");
        let attachment = AttachmentRef {
            id: "doc-verified".to_string(),
            media_type: "text/plain".to_string(),
            uri: raw_path.display().to_string(),
            file_name: Some("doc.txt".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: Some(text_path.display().to_string()),
            text_sha256: Some(sha256_hex(b"derived ok")),
            text_byte_length: Some("derived ok".len() as u64),
            preview_image_uri: None,
            preview_image_media_type: None,
            preview_image_sha256: None,
            preview_image_byte_length: None,
        };
        let cache = AttachmentRenderCache::default();
        assert!(
            load_document_attachment_text(&attachment, None, &cache)
                .expect("initial document should load")
                .is_some()
        );
        fs::write(&text_path, b"derived bad").expect("tamper text fixture");

        let error = load_document_attachment_text(&attachment, None, &cache)
            .expect_err("hot cache should not hide derived text integrity mismatch");
        assert!(
            error.to_string().contains("attachment integrity mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn cached_preview_still_revalidates_derived_integrity() {
        let root = create_fixture_dir("attachment-preview-integrity-cache").expect("fixture dir");
        let raw_path = root.join("plan.dxf");
        let preview_path = root.join("plan.preview.png");
        let preview_bytes = crate::providers::test_fixtures::MINIMAL_PNG;
        fs::write(&raw_path, b"0\nEOF\n").expect("write raw fixture");
        fs::write(&preview_path, preview_bytes).expect("write preview fixture");
        let attachment = AttachmentRef {
            id: "doc-preview-verified".to_string(),
            media_type: "application/dxf".to_string(),
            uri: raw_path.display().to_string(),
            file_name: Some("plan.dxf".to_string()),
            sha256: None,
            byte_length: None,
            text_uri: None,
            text_sha256: None,
            text_byte_length: None,
            preview_image_uri: Some(preview_path.display().to_string()),
            preview_image_media_type: Some("image/png".to_string()),
            preview_image_sha256: Some(sha256_hex(preview_bytes)),
            preview_image_byte_length: Some(preview_bytes.len() as u64),
        };
        let cache = AttachmentRenderCache::default();
        assert!(
            load_attachment_preview_image(&attachment, None, &cache)
                .expect("initial preview should load")
                .is_some()
        );
        fs::write(&preview_path, b"tampered").expect("tamper preview fixture");

        let error = load_attachment_preview_image(&attachment, None, &cache)
            .expect_err("hot cache should not hide preview integrity mismatch");
        assert!(
            error.to_string().contains("attachment integrity mismatch"),
            "{error:#}"
        );
    }
}
