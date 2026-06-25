use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use tracing::warn;

pub(crate) fn quarantine_corrupt_state_file(path: &Path) -> Result<Option<PathBuf>> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("path has no file name: {}", path.display()))?;
    let timestamp_ms = crate::now_ms();
    for attempt in 0..8u8 {
        let mut quarantined_name = OsString::from(file_name);
        quarantined_name.push(format!(".corrupt-{timestamp_ms}"));
        if attempt > 0 {
            quarantined_name.push(format!("-{attempt}"));
        }
        let quarantined_path = path.with_file_name(quarantined_name);
        match fs::rename(path, &quarantined_path) {
            Ok(()) => return Ok(Some(quarantined_path)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                warn!(
                    path = %path.display(),
                    error = ?error,
                    "failed to quarantine corrupted daemon state file"
                );
                return Ok(None);
            }
        }
    }
    warn!(
        path = %path.display(),
        "failed to quarantine corrupted daemon state file after repeated name collisions"
    );
    Ok(None)
}

pub(crate) fn read_json_or_quarantine<T>(path: &Path, state_kind: &'static str) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {} {}", state_kind, path.display()));
        }
    };

    match serde_json::from_slice::<T>(&bytes) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            let quarantined_path = quarantine_corrupt_state_file(path)?;
            warn!(
                state_kind,
                path = %path.display(),
                quarantined_path = quarantined_path.as_ref().map(|value| value.display().to_string()),
                error = %error,
                "ignoring corrupted daemon state file"
            );
            Ok(None)
        }
    }
}
