use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const SAFE_STORAGE_NAMESPACE: &str = "__safe";

/// Encodes one external identifier into a filesystem-safe storage stem.
#[must_use]
pub fn safe_storage_name(id: &str) -> String {
    format!("id-{}", hex::encode(id.as_bytes()))
}

/// Decodes one filesystem-safe storage stem back into its external identifier.
#[must_use]
pub fn decode_safe_storage_name(name: &str) -> Option<String> {
    let encoded = name.strip_prefix("id-")?;
    let bytes = hex::decode(encoded).ok()?;
    String::from_utf8(bytes).ok()
}

/// Returns the legacy raw filename when the identifier was already one safe
/// path component on disk.
#[must_use]
pub fn legacy_storage_name(id: &str) -> Option<String> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id == SAFE_STORAGE_NAMESPACE
        || id.contains('/')
        || id.contains('\\')
    {
        return None;
    }
    Some(id.to_string())
}

/// Returns the canonical safe storage path for one external identifier.
#[must_use]
pub fn safe_storage_path(root: &Path, id: &str, extension: &str) -> PathBuf {
    root.join(SAFE_STORAGE_NAMESPACE).join(format!(
        "{}.{}",
        safe_storage_name(id),
        extension.trim_start_matches('.')
    ))
}

/// Returns the canonical safe storage directory for one external identifier.
#[must_use]
pub fn safe_storage_dir(root: &Path, id: &str) -> PathBuf {
    root.join(SAFE_STORAGE_NAMESPACE)
        .join(safe_storage_name(id))
}

/// Returns the legacy storage path that used the raw identifier verbatim.
#[must_use]
pub fn legacy_storage_path(root: &Path, id: &str, extension: &str) -> Option<PathBuf> {
    Some(root.join(format!(
        "{}.{}",
        legacy_storage_name(id)?,
        extension.trim_start_matches('.')
    )))
}

/// Returns the legacy raw directory path when the identifier was already one
/// safe path component on disk.
#[must_use]
pub fn legacy_storage_dir(root: &Path, id: &str) -> Option<PathBuf> {
    Some(root.join(legacy_storage_name(id)?))
}

/// Resolves the storage path for reading, preferring the new safe path and
/// falling back to the legacy raw-id path for backward compatibility.
#[must_use]
pub fn resolve_storage_path_for_read(root: &Path, id: &str, extension: &str) -> PathBuf {
    let safe = safe_storage_path(root, id, extension);
    if safe.exists() {
        return safe;
    }
    if let Some(legacy) = legacy_storage_path(root, id, extension) {
        if legacy.exists() {
            return legacy;
        }
    }
    safe
}

/// Prepares the storage path for writing. If a legacy raw-id file exists and no
/// safe path exists yet, it is migrated in place to the safe path before write.
pub fn prepare_storage_path_for_write(root: &Path, id: &str, extension: &str) -> Result<PathBuf> {
    let safe = safe_storage_path(root, id, extension);
    if let Some(legacy) = legacy_storage_path(root, id, extension) {
        if !safe.exists() && legacy.exists() {
            if let Some(parent) = safe.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create directory {}", parent.display()))?;
            }
            fs::rename(&legacy, &safe).with_context(|| {
                format!(
                    "failed to migrate legacy storage file {} to {}",
                    legacy.display(),
                    safe.display()
                )
            })?;
        }
    }
    Ok(safe)
}

/// Resolves the storage directory for reading, preferring the namespaced safe
/// path and falling back to the legacy raw-id directory when safe.
#[must_use]
pub fn resolve_storage_dir_for_read(root: &Path, id: &str) -> PathBuf {
    let safe = safe_storage_dir(root, id);
    if safe.exists() {
        return safe;
    }
    if let Some(legacy) = legacy_storage_dir(root, id) {
        if legacy.exists() {
            return legacy;
        }
    }
    safe
}

/// Prepares the storage directory for writing. If a legacy raw-id directory
/// exists and no safe path exists yet, it is migrated in place first.
pub fn prepare_storage_dir_for_write(root: &Path, id: &str) -> Result<PathBuf> {
    let safe = safe_storage_dir(root, id);
    if let Some(legacy) = legacy_storage_dir(root, id) {
        if !safe.exists() && legacy.exists() {
            if let Some(parent) = safe.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create directory {}", parent.display()))?;
            }
            fs::rename(&legacy, &safe).with_context(|| {
                format!(
                    "failed to migrate legacy storage directory {} to {}",
                    legacy.display(),
                    safe.display()
                )
            })?;
        }
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    #[test]
    fn safe_storage_name_encodes_path_separators() {
        let encoded = safe_storage_name("../../../etc/passwd");
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('\\'));
        assert!(encoded.starts_with("id-"));
    }

    #[test]
    fn decode_safe_storage_name_roundtrips_identifiers() {
        let original = "../../../etc/passwd";
        let encoded = safe_storage_name(original);
        assert_eq!(
            decode_safe_storage_name(&encoded).as_deref(),
            Some(original)
        );
    }

    #[test]
    fn safe_storage_path_stays_under_root() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = safe_storage_path(root.path(), "../../../etc/passwd", "jsonl");
        assert!(path.starts_with(root.path()));
        assert!(
            path.parent()
                .expect("safe parent")
                .ends_with(SAFE_STORAGE_NAMESPACE)
        );
        Ok(())
    }

    #[test]
    fn prepare_storage_path_for_write_migrates_legacy_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = legacy_storage_path(root.path(), "session-a", "jsonl").expect("legacy path");
        fs::write(&legacy, "payload")?;

        let safe = prepare_storage_path_for_write(root.path(), "session-a", "jsonl")?;
        assert!(safe.exists());
        assert!(!legacy.exists());
        assert_eq!(fs::read_to_string(safe)?, "payload");
        Ok(())
    }

    #[test]
    fn safe_storage_name_does_not_collide_with_legacy_hex_like_ids() {
        assert_ne!(safe_storage_name("abc"), "616263");
    }

    #[test]
    fn unsafe_legacy_ids_do_not_map_to_paths() {
        let root = Path::new("/tmp/root");
        assert!(legacy_storage_path(root, "../../../etc/passwd", "jsonl").is_none());
        assert!(legacy_storage_path(root, "/tmp/passwd", "jsonl").is_none());
        assert!(legacy_storage_path(root, "__safe", "jsonl").is_none());
    }
}
