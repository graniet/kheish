use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CString;
use std::io::{Read, Write};
#[cfg(unix)]
use std::path::Component as PathComponent;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::ffi::OsStrExt,
    },
};

use anyhow::{Context, Result, anyhow, bail};
use kheish_runtime::{ToolContext, ToolSchema, ToolSchemaField};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(not(unix))]
use tempfile::NamedTempFile;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[cfg(unix)]
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Shared configuration for the default coding tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingToolConfig {
    /// The workspace root enforced by file and shell tools.
    pub workspace_root: PathBuf,
    /// The shell executable used by the Bash tool.
    pub shell: String,
    /// The user agent sent by outbound HTTP fetches.
    pub user_agent: String,
    /// The maximum number of file bytes returned by `read_file`.
    pub max_read_bytes: usize,
    /// The maximum number of results returned by search tools.
    pub max_results: usize,
    /// The maximum file size accepted by in-place text editing tools.
    pub max_edit_bytes: usize,
}

impl CodingToolConfig {
    /// Creates a configuration rooted at the provided workspace path.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            shell: "/bin/bash".to_string(),
            user_agent: "kheish/0.1".to_string(),
            max_read_bytes: 128 * 1024,
            max_results: 200,
            max_edit_bytes: 4 * 1024 * 1024,
        }
    }
}

pub(crate) struct SharedConfig {
    pub(crate) root: PathBuf,
    pub(crate) shell: String,
    pub(crate) user_agent: String,
    pub(crate) max_read_bytes: usize,
    pub(crate) max_results: usize,
    pub(crate) max_edit_bytes: usize,
    pub(crate) http: reqwest::Client,
    file_locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
}

impl SharedConfig {
    pub(crate) fn new(config: CodingToolConfig) -> Self {
        let user_agent = config.user_agent.clone();
        let http = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("reqwest client should build");
        let root = canonicalize_if_exists(&config.workspace_root)
            .unwrap_or_else(|_| config.workspace_root.clone());
        Self {
            root,
            shell: config.shell,
            user_agent,
            max_read_bytes: config.max_read_bytes,
            max_results: config.max_results,
            max_edit_bytes: config.max_edit_bytes,
            http,
            file_locks: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn workspace<'a>(&'a self, ctx: &ToolContext) -> WorkspaceView<'a> {
        let root = ctx
            .metadata
            .get("workspace_root")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|root| canonicalize_if_exists(&root).unwrap_or(root))
            .unwrap_or_else(|| self.root.clone());
        WorkspaceView {
            _shared: Some(self),
            root,
        }
    }

    pub(crate) async fn lock_path(&self, path: &Path) -> Result<PathLockGuard> {
        let lock = {
            let mut locks = self.file_locks.lock().await;
            locks
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let memory_guard = lock.lock_owned().await;
        let process_guard = acquire_process_path_lock(path.to_path_buf()).await?;
        Ok(PathLockGuard {
            _memory_guard: memory_guard,
            _process_guard: process_guard,
        })
    }
}

pub(crate) struct PathLockGuard {
    _memory_guard: OwnedMutexGuard<()>,
    _process_guard: Option<ProcessPathLock>,
}

#[cfg(unix)]
struct ProcessPathLock {
    file: std::fs::File,
}

#[cfg(not(unix))]
struct ProcessPathLock;

async fn acquire_process_path_lock(path: PathBuf) -> Result<Option<ProcessPathLock>> {
    #[cfg(unix)]
    {
        tokio::task::spawn_blocking(move || ProcessPathLock::acquire(&path))
            .await
            .context("process file lock task panicked")?
            .map(Some)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

#[cfg(unix)]
impl ProcessPathLock {
    fn acquire(path: &Path) -> Result<Self> {
        let lock_path = process_lock_path(path);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {}", path.display()));
        }
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for ProcessPathLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(unix)]
fn process_lock_path(path: &Path) -> PathBuf {
    let digest = sha256_hex(path.to_string_lossy().as_bytes());
    std::env::temp_dir()
        .join("kheish-coding-tools-locks")
        .join(format!("{digest}.lock"))
}

pub(crate) struct WorkspaceView<'a> {
    _shared: Option<&'a SharedConfig>,
    root: PathBuf,
}

pub(crate) struct ResolvedWorkspaceDir {
    pub(crate) path: PathBuf,
    #[cfg(unix)]
    pub(crate) dir: File,
}

impl WorkspaceView<'_> {
    pub(crate) fn from_root(root: PathBuf) -> Self {
        Self {
            _shared: None,
            root: canonicalize_if_exists(&root).unwrap_or(root),
        }
    }

    pub(crate) fn resolve_existing_path(&self, raw: &str) -> Result<PathBuf> {
        let path = self.resolve_path(raw)?;
        std::fs::canonicalize(&path)
            .map_err(Into::into)
            .and_then(|resolved| self.ensure_within_root(&resolved))
    }

    pub(crate) fn resolve_write_path(&self, raw: &str) -> Result<PathBuf> {
        let candidate = self.candidate_path(raw)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&candidate) {
            if metadata.file_type().is_symlink() {
                bail!("refusing to modify symlink path {}", candidate.display());
            }
            if metadata.is_dir() {
                bail!("refusing to modify directory path {}", candidate.display());
            }
            self.ensure_within_root(&candidate)?;
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", candidate.display()))?;
        let resolved_parent = normalize_candidate_path(parent)?;
        self.ensure_within_root(&resolved_parent)?;
        let file_name = candidate
            .file_name()
            .ok_or_else(|| anyhow!("path has no file name: {}", candidate.display()))?;
        Ok(resolved_parent.join(file_name))
    }

    pub(crate) fn resolve_base_dir(&self, raw: Option<&str>) -> Result<PathBuf> {
        match raw {
            Some(value) if !value.is_empty() => self.resolve_existing_path(value),
            _ => self.ensure_within_root(&self.root),
        }
    }

    pub(crate) fn resolve_base_dir_no_follow(
        &self,
        raw: Option<&str>,
    ) -> Result<ResolvedWorkspaceDir> {
        #[cfg(unix)]
        {
            let root = canonicalize_if_exists(&self.root)?;
            let components = workspace_dir_relative_components_no_follow(&root, raw)?;
            let dir = open_workspace_parent_dir(&root, &components, false)?;
            let mut path = root;
            for component in &components {
                path.push(component);
            }
            Ok(ResolvedWorkspaceDir { path, dir })
        }

        #[cfg(not(unix))]
        {
            Ok(ResolvedWorkspaceDir {
                path: self.resolve_base_dir(raw)?,
            })
        }
    }

    pub(crate) fn workspace_relative_string(&self, path: &Path) -> String {
        normalize_candidate_path(path)
            .ok()
            .and_then(|normalized| normalized.strip_prefix(&self.root).ok().map(PathBuf::from))
            .unwrap_or_else(|| path.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/")
    }

    pub(crate) fn root(&self) -> Result<PathBuf> {
        canonicalize_if_exists(&self.root)
    }

    fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
        self.ensure_within_root(&self.candidate_path(raw)?)
    }

    fn candidate_path(&self, raw: &str) -> Result<PathBuf> {
        let candidate = PathBuf::from(raw);
        if candidate.is_absolute() {
            return Ok(candidate);
        }
        Ok(self.root.join(normalize_relative_path(raw)?))
    }

    fn ensure_within_root(&self, candidate: &Path) -> Result<PathBuf> {
        let root = canonicalize_if_exists(&self.root)?;
        let candidate = normalize_candidate_path(candidate)?;
        if candidate.starts_with(&root) {
            Ok(candidate)
        } else {
            bail!(
                "path {} escapes workspace root {}",
                candidate.display(),
                root.display()
            )
        }
    }
}

fn canonicalize_if_exists(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        Ok(std::fs::canonicalize(path)?)
    } else {
        Ok(path.to_path_buf())
    }
}

fn normalize_candidate_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }

    let mut suffix = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            bail!("path has no existing ancestor: {}", path.display());
        };
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow!("path has no existing ancestor: {}", path.display()))?;
    }

    let mut normalized = std::fs::canonicalize(ancestor)?;
    for component in suffix.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

fn normalize_relative_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes workspace root: {raw}");
                }
            }
            _ => bail!("unsupported path component in {raw}"),
        }
    }
    Ok(normalized)
}

#[cfg(unix)]
fn workspace_dir_relative_components_no_follow(
    root: &Path,
    raw: Option<&str>,
) -> Result<Vec<std::ffi::OsString>> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let raw_path = Path::new(raw);
    let relative = if raw_path.is_absolute() {
        let normalized = normalize_absolute_path_lexical(raw_path)?;
        normalized
            .strip_prefix(root)
            .with_context(|| {
                format!(
                    "path {} escapes workspace root {}",
                    normalized.display(),
                    root.display()
                )
            })?
            .to_path_buf()
    } else {
        normalize_relative_path(raw)?
    };
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            PathComponent::Normal(part) => components.push(part.to_os_string()),
            PathComponent::CurDir => {}
            _ => bail!("unsupported path component in {raw}"),
        }
    }
    Ok(components)
}

#[cfg(unix)]
fn normalize_absolute_path_lexical(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            PathComponent::RootDir => normalized.push("/"),
            PathComponent::CurDir => {}
            PathComponent::Normal(part) => normalized.push(part),
            PathComponent::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes filesystem root: {}", path.display());
                }
            }
            _ => bail!("unsupported path component in {}", path.display()),
        }
    }
    Ok(normalized)
}

pub(crate) fn string_field(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("missing string field `{key}`"))
}

pub(crate) fn optional_string_field(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

pub(crate) fn optional_usize_field(input: &Value, key: &str) -> Option<usize> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

pub(crate) fn optional_bool_field(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

pub(crate) fn optional_string_array_field(input: &Value, key: &str) -> Option<Vec<String>> {
    input.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect()
    })
}

pub(crate) fn clamp_limit(requested: Option<usize>, max_results: usize) -> usize {
    requested.unwrap_or(max_results).min(max_results)
}

pub(crate) fn truncate_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut truncated = String::new();
    for ch in text.chars() {
        if truncated.len() + ch.len_utf8() > max_bytes {
            break;
        }
        truncated.push(ch);
    }
    (truncated, true)
}

pub(crate) async fn read_file_bytes_limited(
    workspace_root: PathBuf,
    path: PathBuf,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    tokio::task::spawn_blocking(move || {
        read_file_bytes_limited_blocking(&workspace_root, &path, max_bytes)
    })
    .await
    .context("limited file reader task panicked")?
}

fn read_file_bytes_limited_blocking(
    workspace_root: &Path,
    path: &Path,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    #[cfg(unix)]
    let mut file = open_workspace_file_no_follow(workspace_root, path)?;
    #[cfg(not(unix))]
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;

    let mut reader = (&mut file).take(max_bytes.saturating_add(1) as u64);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
    }
    Ok((bytes, truncated))
}

pub(crate) async fn read_text_file_for_edit(
    workspace_root: PathBuf,
    path: PathBuf,
    max_bytes: usize,
) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let bytes = read_text_file_bytes_for_edit_blocking(&workspace_root, &path, max_bytes)?;
        String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))
    })
    .await
    .context("edit file reader task panicked")?
}

fn read_text_file_bytes_for_edit_blocking(
    workspace_root: &Path,
    path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    #[cfg(unix)]
    let mut file = open_workspace_file_no_follow(workspace_root, path)?;
    #[cfg(not(unix))]
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;

    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.len() > max_bytes as u64 {
        bail!(
            "refusing to edit {} because it is larger than {} bytes",
            path.display(),
            max_bytes
        );
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(bytes)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) async fn assert_expected_sha256(
    workspace_root: PathBuf,
    path: PathBuf,
    expected: Option<&str>,
) -> Result<()> {
    let Some(expected) = expected.map(ToString::to_string) else {
        return Ok(());
    };
    let display_path = path.display().to_string();
    let bytes = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        let mut file = open_workspace_file_no_follow(&workspace_root, &path)?;
        #[cfg(not(unix))]
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).with_context(|| {
            format!(
                "failed to read {} for expected_sha256 check",
                path.display()
            )
        })?;
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("expected_sha256 reader task panicked")??;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!(
            "expected_sha256 mismatch for {}: expected {}, actual {}",
            display_path,
            expected,
            actual
        );
    }
    Ok(())
}

pub(crate) async fn atomic_write_file(
    workspace_root: PathBuf,
    path: PathBuf,
    content: Vec<u8>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        atomic_write_file_blocking(&workspace_root, &path, &content)
    })
    .await
    .context("atomic file writer task panicked")?
}

pub(crate) struct PreparedAtomicWrite {
    path: PathBuf,
    #[cfg(unix)]
    parent: File,
    #[cfg(unix)]
    temp_leaf: CString,
    #[cfg(unix)]
    final_leaf: CString,
    #[cfg(not(unix))]
    workspace_root: PathBuf,
    #[cfg(not(unix))]
    content: Vec<u8>,
}

pub(crate) async fn prepare_atomic_write_file(
    workspace_root: PathBuf,
    path: PathBuf,
    content: Vec<u8>,
) -> Result<PreparedAtomicWrite> {
    tokio::task::spawn_blocking(move || {
        prepare_atomic_write_file_blocking(&workspace_root, &path, content)
    })
    .await
    .context("atomic file writer preparation task panicked")?
}

pub(crate) async fn commit_prepared_atomic_write(prepared: PreparedAtomicWrite) -> Result<()> {
    tokio::task::spawn_blocking(move || prepared.commit_blocking())
        .await
        .context("atomic file writer commit task panicked")?
}

pub(crate) async fn abort_prepared_atomic_write(prepared: PreparedAtomicWrite) {
    let _ = tokio::task::spawn_blocking(move || prepared.abort_blocking()).await;
}

fn atomic_write_file_blocking(workspace_root: &Path, path: &Path, content: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        atomic_write_file_blocking_unix(workspace_root, path, content)
    }

    #[cfg(not(unix))]
    {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() {
                bail!("refusing to modify symlink path {}", path.display());
            }
            if metadata.is_dir() {
                bail!("refusing to modify directory path {}", path.display());
            }
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let resolved_parent = std::fs::canonicalize(parent)
            .with_context(|| format!("failed to resolve {}", parent.display()))?;
        let resolved_root = canonicalize_if_exists(workspace_root)?;
        if !resolved_parent.starts_with(&resolved_root) {
            bail!(
                "path {} escapes workspace root {}",
                resolved_parent.display(),
                resolved_root.display()
            );
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?;
        let final_path = resolved_parent.join(file_name);
        if let Ok(metadata) = std::fs::symlink_metadata(&final_path) {
            if metadata.file_type().is_symlink() {
                bail!("refusing to modify symlink path {}", final_path.display());
            }
            if metadata.is_dir() {
                bail!("refusing to modify directory path {}", final_path.display());
            }
        }
        let mut temp = NamedTempFile::new_in(&resolved_parent).with_context(|| {
            format!(
                "failed to create temporary file in {}",
                resolved_parent.display()
            )
        })?;
        temp.write_all(content).with_context(|| {
            format!(
                "failed to write temporary file for {}",
                final_path.display()
            )
        })?;
        temp.as_file_mut().sync_all().with_context(|| {
            format!("failed to sync temporary file for {}", final_path.display())
        })?;
        temp.persist(&final_path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to atomically replace {}", final_path.display()))?;
        sync_parent_dir(&resolved_parent);
        Ok(())
    }
}

fn prepare_atomic_write_file_blocking(
    workspace_root: &Path,
    path: &Path,
    content: Vec<u8>,
) -> Result<PreparedAtomicWrite> {
    #[cfg(unix)]
    {
        prepare_atomic_write_file_blocking_unix(workspace_root, path, &content)
    }

    #[cfg(not(unix))]
    {
        Ok(PreparedAtomicWrite {
            path: path.to_path_buf(),
            workspace_root: workspace_root.to_path_buf(),
            content,
        })
    }
}

#[cfg(unix)]
fn open_workspace_file_no_follow(workspace_root: &Path, path: &Path) -> Result<File> {
    let root = canonicalize_if_exists(workspace_root)?;
    let relative_components = workspace_relative_components(&root, path)?;
    let (parent_components, leaf) = split_parent_and_leaf(&relative_components, path)?;
    let parent = open_workspace_parent_dir(&root, parent_components, false)?;
    let leaf_c = cstring_for_component(leaf)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to open {}", path.display()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.is_dir() {
        bail!("refusing to read directory path {}", path.display());
    }
    Ok(file)
}

#[cfg(unix)]
fn atomic_write_file_blocking_unix(
    workspace_root: &Path,
    path: &Path,
    content: &[u8],
) -> Result<()> {
    let root = canonicalize_if_exists(workspace_root)?;
    let relative_components = workspace_relative_components(&root, path)?;
    let (parent_components, leaf) = split_parent_and_leaf(&relative_components, path)?;
    let parent = open_workspace_parent_dir(&root, parent_components, true)?;
    let leaf_c = cstring_for_component(leaf)?;
    reject_existing_symlink_or_directory_at(parent.as_raw_fd(), &leaf_c, path)?;

    let temp_name = temporary_leaf_name(leaf);
    let temp_c = cstring_for_component(Path::new(&temp_name).as_os_str())?;
    let temp_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temp_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if temp_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to create temporary file for {}", path.display()));
    }
    let mut temp = unsafe { File::from_raw_fd(temp_fd) };
    let write_result = (|| -> Result<()> {
        temp.write_all(content)
            .with_context(|| format!("failed to write temporary file for {}", path.display()))?;
        temp.sync_all()
            .with_context(|| format!("failed to sync temporary file for {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), temp_c.as_ptr(), 0) };
        return Err(error);
    }
    drop(temp);

    let renamed = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            temp_c.as_ptr(),
            parent.as_raw_fd(),
            leaf_c.as_ptr(),
        )
    };
    if renamed != 0 {
        let error = std::io::Error::last_os_error();
        let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), temp_c.as_ptr(), 0) };
        return Err(error)
            .with_context(|| format!("failed to atomically replace {}", path.display()));
    }
    parent
        .sync_all()
        .with_context(|| format!("failed to sync parent directory for {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn prepare_atomic_write_file_blocking_unix(
    workspace_root: &Path,
    path: &Path,
    content: &[u8],
) -> Result<PreparedAtomicWrite> {
    let root = canonicalize_if_exists(workspace_root)?;
    let relative_components = workspace_relative_components(&root, path)?;
    let (parent_components, leaf) = split_parent_and_leaf(&relative_components, path)?;
    let parent = open_workspace_parent_dir(&root, parent_components, true)?;
    let final_leaf = cstring_for_component(leaf)?;
    reject_existing_symlink_or_directory_at(parent.as_raw_fd(), &final_leaf, path)?;

    let temp_name = temporary_leaf_name(leaf);
    let temp_leaf = cstring_for_component(Path::new(&temp_name).as_os_str())?;
    let temp_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temp_leaf.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if temp_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to create temporary file for {}", path.display()));
    }
    let mut temp = unsafe { File::from_raw_fd(temp_fd) };
    let write_result = (|| -> Result<()> {
        temp.write_all(content)
            .with_context(|| format!("failed to write temporary file for {}", path.display()))?;
        temp.sync_all()
            .with_context(|| format!("failed to sync temporary file for {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), temp_leaf.as_ptr(), 0) };
        return Err(error);
    }
    drop(temp);

    Ok(PreparedAtomicWrite {
        path: path.to_path_buf(),
        parent,
        temp_leaf,
        final_leaf,
    })
}

impl PreparedAtomicWrite {
    fn commit_blocking(self) -> Result<()> {
        #[cfg(unix)]
        {
            let renamed = unsafe {
                libc::renameat(
                    self.parent.as_raw_fd(),
                    self.temp_leaf.as_ptr(),
                    self.parent.as_raw_fd(),
                    self.final_leaf.as_ptr(),
                )
            };
            if renamed != 0 {
                let error = std::io::Error::last_os_error();
                let _ =
                    unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.temp_leaf.as_ptr(), 0) };
                return Err(error).with_context(|| {
                    format!("failed to atomically replace {}", self.path.display())
                });
            }
            self.parent.sync_all().with_context(|| {
                format!(
                    "failed to sync parent directory for {}",
                    self.path.display()
                )
            })?;
            Ok(())
        }

        #[cfg(not(unix))]
        {
            atomic_write_file_blocking(&self.workspace_root, &self.path, &self.content)
        }
    }

    fn abort_blocking(self) {
        #[cfg(unix)]
        {
            let _ = unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.temp_leaf.as_ptr(), 0) };
        }
    }
}

#[cfg(unix)]
fn open_workspace_parent_dir(
    root: &Path,
    parent_components: &[std::ffi::OsString],
    create_missing: bool,
) -> Result<File> {
    let root_c = CString::new(root.as_os_str().as_bytes())
        .with_context(|| format!("path contains NUL byte: {}", root.display()))?;
    let root_fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to open workspace root {}", root.display()));
    }
    let mut current = unsafe { File::from_raw_fd(root_fd) };
    for component in parent_components {
        let component_c = cstring_for_component(component.as_os_str())?;
        let mut next_fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                component_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if next_fd < 0 && create_missing && last_errno() == libc::ENOENT {
            let mkdir_result =
                unsafe { libc::mkdirat(current.as_raw_fd(), component_c.as_ptr(), 0o777) };
            if mkdir_result != 0 && last_errno() != libc::EEXIST {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("failed to create directory component {:?}", component)
                });
            }
            next_fd = unsafe {
                libc::openat(
                    current.as_raw_fd(),
                    component_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
        }
        if next_fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open directory component {:?}", component));
        }
        current = unsafe { File::from_raw_fd(next_fd) };
    }
    Ok(current)
}

#[cfg(unix)]
fn workspace_relative_components(root: &Path, path: &Path) -> Result<Vec<std::ffi::OsString>> {
    let normalized = normalize_candidate_path(path)?;
    let relative = normalized.strip_prefix(root).with_context(|| {
        format!(
            "path {} escapes workspace root {}",
            normalized.display(),
            root.display()
        )
    })?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            PathComponent::Normal(part) => components.push(part.to_os_string()),
            PathComponent::CurDir => {}
            _ => bail!("unsupported path component in {}", path.display()),
        }
    }
    if components.is_empty() {
        bail!("path has no file name: {}", path.display());
    }
    Ok(components)
}

#[cfg(unix)]
fn split_parent_and_leaf<'a>(
    components: &'a [std::ffi::OsString],
    path: &Path,
) -> Result<(&'a [std::ffi::OsString], &'a std::ffi::OsStr)> {
    let Some((leaf, parents)) = components.split_last() else {
        bail!("path has no file name: {}", path.display());
    };
    Ok((parents, leaf.as_os_str()))
}

#[cfg(unix)]
fn cstring_for_component(component: &std::ffi::OsStr) -> Result<CString> {
    let bytes = component.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'/') {
        bail!("unsupported path component {:?}", component);
    }
    CString::new(bytes).context("failed to prepare path component")
}

#[cfg(unix)]
fn reject_existing_symlink_or_directory_at(
    parent_fd: RawFd,
    leaf: &CString,
    path: &Path,
) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            leaf.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        if last_errno() == libc::ENOENT {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to stat {}", path.display()));
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        bail!("refusing to modify symlink path {}", path.display());
    }
    if file_type == libc::S_IFDIR {
        bail!("refusing to modify directory path {}", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn temporary_leaf_name(leaf: &std::ffi::OsStr) -> String {
    let digest = sha256_hex(leaf.as_bytes());
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        ".kheish-tmp-{}-{}-{}",
        std::process::id(),
        counter,
        &digest[..16]
    )
}

#[cfg(unix)]
fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default()
}

#[cfg(not(unix))]
fn sync_parent_dir(parent: &Path) {
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
}

pub(crate) fn tool_schema(fields: Vec<ToolSchemaField>) -> ToolSchema {
    ToolSchema { fields }
}
