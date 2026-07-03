use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

fn ensure_parent_dir(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    Ok(parent.to_path_buf())
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = ensure_parent_dir(path)?;
    File::open(&parent)
        .with_context(|| format!("failed to open directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", parent.display()))
}

pub(crate) fn temp_path_for(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?;
    let nonce = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let temp_name = OsString::from(format!(
        ".{}.tmp-{}-{}",
        file_name.to_string_lossy(),
        std::process::id(),
        nonce
    ));
    Ok(path.with_file_name(temp_name))
}

/// Writes bytes atomically by syncing a temp file before renaming it into place.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_parent_dir(path)?;
    let temp_path = temp_path_for(path)?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("failed to open {}", temp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, path).with_context(|| {
            format!(
                "failed to replace {} with {}",
                path.display(),
                temp_path.display()
            )
        })?;
        sync_parent_dir(path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    result
}

/// Serializes one value as pretty JSON and writes it atomically.
pub fn write_json_pretty_atomically<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

#[cfg(unix)]
fn append_all_once(file: &File, payload: &[u8], path: &Path) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    let written = unsafe { libc::write(file.as_raw_fd(), payload.as_ptr().cast(), payload.len()) };
    if written < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to append {}", path.display()));
    }
    let written = usize::try_from(written).expect("libc::write returned a negative size");
    if written != payload.len() {
        return Err(anyhow!(
            "short append to {}: wrote {} of {} bytes",
            path.display(),
            written,
            payload.len()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn append_all_once(file: &mut File, payload: &[u8], path: &Path) -> Result<()> {
    file.write_all(payload)
        .with_context(|| format!("failed to append {}", path.display()))
}

/// Truncates the bytes after the final newline — the torn trailing line an
/// interrupted append leaves behind. Without this, the next append would glue
/// its first record onto the partial line, corrupting that record too and
/// turning a recoverable trailing tear into a mid-file parse error.
fn heal_torn_trailing_line(file: &File, path: &Path) -> Result<()> {
    let len = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    if len == 0 {
        return Ok(());
    }

    let mut last = [0u8; 1];
    (&*file)
        .seek(SeekFrom::End(-1))
        .and_then(|_| (&*file).read_exact(&mut last))
        .with_context(|| format!("failed to read tail of {}", path.display()))?;
    if last[0] == b'\n' {
        return Ok(());
    }

    // Scan backwards in chunks for the last newline; everything after it is
    // the torn line.
    const CHUNK: u64 = 4096;
    let mut keep = 0u64;
    let mut cursor = len;
    let mut buffer = [0u8; CHUNK as usize];
    'scan: while cursor > 0 {
        let start = cursor.saturating_sub(CHUNK);
        let span = (cursor - start) as usize;
        (&*file)
            .seek(SeekFrom::Start(start))
            .and_then(|_| (&*file).read_exact(&mut buffer[..span]))
            .with_context(|| format!("failed to scan tail of {}", path.display()))?;
        for index in (0..span).rev() {
            if buffer[index] == b'\n' {
                keep = start + index as u64 + 1;
                break 'scan;
            }
        }
        cursor = start;
    }

    tracing::warn!(
        path = %path.display(),
        torn_bytes = len - keep,
        "dropping torn trailing line before append (likely an interrupted write)"
    );
    file.set_len(keep)
        .with_context(|| format!("failed to truncate torn line in {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(())
}

/// Appends one or more JSON values as newline-delimited records and syncs the file contents.
pub fn append_json_lines_sync<T>(path: &Path, values: &[T]) -> Result<()>
where
    T: Serialize,
{
    if values.is_empty() {
        return Ok(());
    }

    ensure_parent_dir(path)?;
    let existed = path.exists();
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    #[cfg(unix)]
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    heal_torn_trailing_line(&file, path)?;
    let len_before_append = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    let mut payload = Vec::new();
    for value in values {
        serde_json::to_writer(&mut payload, value)
            .with_context(|| format!("failed to append {}", path.display()))?;
        payload.push(b'\n');
    }
    #[cfg(unix)]
    let appended = append_all_once(&file, &payload, path);
    #[cfg(not(unix))]
    let appended = append_all_once(&mut file, &payload, path);
    if appended.is_err() {
        // Roll a short write back to the pre-append length so the file keeps
        // ending on a complete line; a crash between write and truncate still
        // leaves only a trailing tear, which the next append heals.
        let _ = file.set_len(len_before_append);
    }
    appended?;
    file.sync_data()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    if !existed {
        sync_parent_dir(path)?;
    }
    Ok(())
}

/// Appends one JSON value as a line and syncs the file contents before returning.
pub fn append_json_line_sync<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    append_json_lines_sync(path, std::slice::from_ref(value))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::{
        append_json_line_sync, append_json_lines_sync, atomic_write, write_json_pretty_atomically,
    };

    #[test]
    fn atomic_write_replaces_existing_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state.json");
        std::fs::write(&path, b"old")?;

        atomic_write(&path, b"new")?;

        assert_eq!(std::fs::read(&path)?, b"new");
        Ok(())
    }

    #[test]
    fn write_json_pretty_atomically_round_trips() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state.json");

        write_json_pretty_atomically(&path, &json!({"ok": true, "count": 2}))?;

        let raw = std::fs::read_to_string(&path)?;
        assert!(raw.contains("\"ok\": true"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&raw)?,
            json!({"ok": true, "count": 2})
        );
        Ok(())
    }

    #[test]
    fn append_json_line_sync_writes_newline_delimited_json() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("events.jsonl");

        append_json_line_sync(&path, &json!({"offset": 1}))?;
        append_json_line_sync(&path, &json!({"offset": 2}))?;

        let raw = std::fs::read_to_string(&path)?;
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines
                .into_iter()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            vec![json!({"offset": 1}), json!({"offset": 2})]
        );
        Ok(())
    }

    #[test]
    fn append_json_lines_sync_writes_batched_json_lines() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("events.jsonl");

        append_json_lines_sync(&path, &[json!({"offset": 1}), json!({"offset": 2})])?;

        let raw = std::fs::read_to_string(&path)?;
        assert_eq!(
            raw.lines()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            vec![json!({"offset": 1}), json!({"offset": 2})]
        );
        Ok(())
    }
}
