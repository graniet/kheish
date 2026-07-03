//! Exclusive state-root lock shared by `serve` and offline maintenance
//! commands (`sessions vacuum`). Holding it guarantees no other daemon or
//! maintenance process is mutating the same state root.
//!
//! The flock-based guarantee is unix-only; on other platforms acquisition is
//! a no-op (matching the historical `serve` behavior).

use anyhow::{Context, Result};
use std::path::Path;

#[cfg(unix)]
pub(crate) struct StateRootLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl StateRootLock {
    pub(crate) fn acquire(state_root: &Path) -> Result<Self> {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        std::fs::create_dir_all(state_root)
            .with_context(|| format!("failed to create state root {}", state_root.display()))?;
        let path = state_root.join("daemon.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open daemon lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::bail!(
                "state root {} is already locked by another daemon: {error}",
                state_root.display()
            );
        }
        writeln!(&file, "pid={}\nmechanism=flock", std::process::id())
            .with_context(|| format!("failed to write daemon lock {}", path.display()))?;
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for StateRootLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
pub(crate) struct StateRootLock;

#[cfg(not(unix))]
impl StateRootLock {
    pub(crate) fn acquire(state_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_root)
            .with_context(|| format!("failed to create state root {}", state_root.display()))?;
        Ok(Self)
    }
}
