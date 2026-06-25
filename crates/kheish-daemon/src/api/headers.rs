use anyhow::{Context as _, Result};
use axum::http::{HeaderMap, HeaderValue, header};

use crate::assets::StoredAssetRecord;

const MAX_DOWNLOAD_FILENAME_CHARS: usize = 180;

pub(crate) fn asset_raw_response_headers(asset: &StoredAssetRecord) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&asset.media_type)
            .context("asset media type is not a valid header")?,
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        asset_content_disposition_header(&asset.id, &asset.file_name, &asset.media_type)?,
    );
    Ok(headers)
}

fn asset_content_disposition_header(
    asset_id: &str,
    file_name: &str,
    media_type: &str,
) -> Result<HeaderValue> {
    let display_name = safe_download_display_name(asset_id, file_name, media_type);
    let fallback = ascii_download_filename(asset_id, &display_name, media_type);
    let mut value = format!("inline; filename=\"{fallback}\"");

    if display_name != fallback {
        value.push_str("; filename*=UTF-8''");
        value.push_str(&urlencoding::encode(&display_name));
    }

    HeaderValue::from_str(&value).context("asset content disposition is not a valid header")
}

fn safe_download_display_name(asset_id: &str, file_name: &str, media_type: &str) -> String {
    let leaf = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name)
        .trim();
    let mut cleaned = String::with_capacity(leaf.len());
    let mut last_was_replacement = false;
    for character in leaf.chars() {
        let replacement = character.is_control()
            || matches!(
                character,
                '"' | '\\' | '/' | ':' | '<' | '>' | '|' | '?' | '*' | ';' | '=' | '\''
            );
        if replacement {
            if !last_was_replacement {
                cleaned.push('_');
                last_was_replacement = true;
            }
        } else {
            cleaned.push(character);
            last_was_replacement = false;
        }
        if cleaned.chars().count() >= MAX_DOWNLOAD_FILENAME_CHARS {
            break;
        }
    }

    let cleaned = cleaned.trim_matches([' ', '.', '_']).to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return default_asset_download_filename(asset_id, media_type);
    }
    cleaned
}

fn ascii_download_filename(asset_id: &str, display_name: &str, media_type: &str) -> String {
    let mut fallback = String::with_capacity(display_name.len());
    let mut last_was_replacement = false;
    for character in display_name.chars() {
        let replacement = !matches!(
            character,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' | ' '
        );
        if replacement {
            if !last_was_replacement {
                fallback.push('_');
                last_was_replacement = true;
            }
        } else {
            fallback.push(character);
            last_was_replacement = false;
        }
        if fallback.chars().count() >= MAX_DOWNLOAD_FILENAME_CHARS {
            break;
        }
    }

    let fallback = fallback.trim_matches([' ', '.', '_']).to_string();
    if fallback.is_empty()
        || !fallback
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
    {
        return default_asset_download_filename(asset_id, media_type);
    }
    fallback
}

fn default_asset_download_filename(asset_id: &str, media_type: &str) -> String {
    format!("{asset_id}.{}", download_extension(media_type))
}

fn download_extension(media_type: &str) -> &'static str {
    match media_type {
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/pdf" => "pdf",
        "application/dxf" => "dxf",
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
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_content_disposition_header_sanitizes_control_and_path_characters() -> Result<()> {
        let value = asset_content_disposition_header(
            "asset-7",
            "../nested\\report\"; filename*=UTF-8''owned\r\n.txt",
            "text/plain",
        )?;
        let value = value.to_str()?;

        assert_eq!(
            value,
            "inline; filename=\"report_ filename_UTF-8_owned_.txt\""
        );
        assert!(!value.contains("nested"));
        assert!(!value.contains('\r'));
        assert!(!value.contains('\n'));
        assert_eq!(value.matches("filename*=").count(), 0);

        Ok(())
    }

    #[test]
    fn asset_content_disposition_header_keeps_utf8_name_via_extended_parameter() -> Result<()> {
        let value = asset_content_disposition_header(
            "asset-8",
            "resume-\u{00e9}t\u{00e9}.txt",
            "text/plain",
        )?;
        let value = value.to_str()?;

        assert!(value.starts_with("inline; filename=\"resume-_t_.txt\""));
        assert!(value.contains("; filename*=UTF-8''resume-%C3%A9t%C3%A9.txt"));

        Ok(())
    }

    #[test]
    fn asset_content_disposition_header_falls_back_for_empty_safe_name() -> Result<()> {
        let value = asset_content_disposition_header("asset-9", "\r\n", "image/png")?;

        assert_eq!(value.to_str()?, "inline; filename=\"asset-9.png\"");

        Ok(())
    }
}
