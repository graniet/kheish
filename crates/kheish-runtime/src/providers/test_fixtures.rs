//! Shared test fixtures for provider attachment coverage.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use sha2::{Digest, Sha256};

use kheish_types::AttachmentRef;

pub(crate) const MINIMAL_PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, b'I', b'D', b'A', b'T', 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92, 0xef, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N',
    b'D', 0xae, 0x42, 0x60, 0x82,
];

pub(crate) const MINIMAL_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08, 0x07, 0x07,
    0x07, 0x09, 0x09, 0x08, 0x0a, 0x0c, 0x14, 0x0d, 0x0c, 0x0b, 0x0b, 0x0c, 0x19, 0x12, 0x13, 0x0f,
    0x14, 0x1d, 0x1a, 0x1f, 0x1e, 0x1d, 0x1a, 0x1c, 0x1c, 0x20, 0x24, 0x2e, 0x27, 0x20, 0x22, 0x2c,
    0x23, 0x1c, 0x1c, 0x28, 0x37, 0x29, 0x2c, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1f, 0x27, 0x39, 0x3d,
    0x38, 0x32, 0x3c, 0x2e, 0x33, 0x34, 0x32, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01,
    0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00, 0x14, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xc4,
    0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00,
    0xff, 0xd9,
];

pub(crate) fn create_fixture_dir(prefix: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = std::env::temp_dir().join(format!("kheish-runtime-{prefix}-{nonce}"));
    fs::create_dir_all(&root)?;
    Ok(root)
}

pub(crate) fn write_image_attachment(
    root: &Path,
    file_name: &str,
    media_type: &str,
) -> Result<AttachmentRef> {
    let bytes = match media_type {
        "image/png" => MINIMAL_PNG,
        "image/jpeg" => MINIMAL_JPEG,
        other => anyhow::bail!("unsupported test media type '{other}'"),
    };
    let path = root.join(file_name);
    fs::write(&path, bytes)?;
    Ok(AttachmentRef {
        id: format!("fixture-{file_name}"),
        media_type: media_type.to_string(),
        uri: path.display().to_string(),
        file_name: Some(file_name.to_string()),
        sha256: Some(hex::encode(Sha256::digest(bytes))),
        byte_length: Some(bytes.len() as u64),
        text_uri: None,
        text_sha256: None,
        text_byte_length: None,
        preview_image_uri: None,
        preview_image_media_type: None,
        preview_image_sha256: None,
        preview_image_byte_length: None,
    })
}

pub(crate) fn write_document_attachment(
    root: &Path,
    file_name: &str,
    media_type: &str,
    text: &str,
) -> Result<AttachmentRef> {
    let raw_path = root.join(file_name);
    let text_path = root.join(format!("{file_name}.txt"));
    fs::write(&raw_path, text.as_bytes())?;
    fs::write(&text_path, text.as_bytes())?;
    Ok(AttachmentRef {
        id: format!("fixture-{file_name}"),
        media_type: media_type.to_string(),
        uri: raw_path.display().to_string(),
        file_name: Some(file_name.to_string()),
        sha256: None,
        byte_length: Some(text.len() as u64),
        text_uri: Some(text_path.display().to_string()),
        text_sha256: None,
        text_byte_length: None,
        preview_image_uri: None,
        preview_image_media_type: None,
        preview_image_sha256: None,
        preview_image_byte_length: None,
    })
}

pub(crate) fn write_document_attachment_with_preview(
    root: &Path,
    file_name: &str,
    media_type: &str,
    text: &str,
) -> Result<AttachmentRef> {
    let raw_path = root.join(file_name);
    let text_path = root.join(format!("{file_name}.txt"));
    let preview_path = root.join(format!("{file_name}.preview.png"));
    fs::write(&raw_path, text.as_bytes())?;
    fs::write(&text_path, text.as_bytes())?;
    fs::write(&preview_path, MINIMAL_PNG)?;
    Ok(AttachmentRef {
        id: format!("fixture-{file_name}"),
        media_type: media_type.to_string(),
        uri: raw_path.display().to_string(),
        file_name: Some(file_name.to_string()),
        sha256: None,
        byte_length: Some(text.len() as u64),
        text_uri: Some(text_path.display().to_string()),
        text_sha256: None,
        text_byte_length: None,
        preview_image_uri: Some(preview_path.display().to_string()),
        preview_image_media_type: Some("image/png".to_string()),
        preview_image_sha256: None,
        preview_image_byte_length: None,
    })
}
