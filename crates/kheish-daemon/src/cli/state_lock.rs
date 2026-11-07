//! Exclusive state-root lock shared by `serve` and offline maintenance
//! commands (`sessions vacuum`). Holding it guarantees no other daemon or
//! maintenance process is mutating the same state root.
//!
//! `serve --state-lock-wait` turns a second daemon into a warm standby: it
//! polls the lock with backoff instead of exiting, and takes over the moment
//! the active daemon dies, since the kernel releases a flock with its holder.
//! flock is not reliable over NFS; put a shared state root on a block volume.
//!
//! The flock-based guarantee is unix-only; on other platforms acquisition is
//! a no-op (matching the historical `serve` behavior).

use anyhow::{Context, Result};
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

/// First delay between two lock attempts of a waiting daemon.
const STATE_LOCK_POLL_INITIAL: Duration = Duration::from_millis(100);
/// Longest delay between two lock attempts of a waiting daemon.
const STATE_LOCK_POLL_MAX: Duration = Duration::from_secs(2);
/// How often a waiting daemon logs that it is still waiting.
const STATE_LOCK_WAIT_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// How long `serve` waits for a state root another daemon holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum StateLockWait {
    /// Exit at once when the state root is locked (the historical behavior).
    #[default]
    Never,
    /// Wait up to this long, then exit.
    For(Duration),
    /// Wait until the lock is released.
    Forever,
}

impl FromStr for StateLockWait {
    type Err = String;

    /// Parses `forever`, `0`, a plain number of milliseconds, or a number
    /// with a `ms`, `s`, `m`, or `h` unit.
    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw == "forever" {
            return Ok(Self::Forever);
        }
        let split = raw
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(raw.len());
        let (digits, unit) = raw.split_at(split);
        let value = digits.parse::<u64>().map_err(|_| {
            format!("invalid state lock wait `{raw}`: use forever, 0, or a duration such as 1500ms, 30s, 5m, or 1h")
        })?;
        let millis = match unit.trim() {
            "" | "ms" => Some(value),
            "s" => value.checked_mul(1_000),
            "m" => value.checked_mul(60_000),
            "h" => value.checked_mul(3_600_000),
            other => {
                return Err(format!(
                    "invalid state lock wait unit `{other}`: use ms, s, m, or h"
                ));
            }
        }
        .ok_or_else(|| format!("state lock wait `{raw}` is too large"))?;
        Ok(if millis == 0 {
            Self::Never
        } else {
            Self::For(Duration::from_millis(millis))
        })
    }
}

impl std::fmt::Display for StateLockWait {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Never => formatter.write_str("0"),
            Self::For(duration) => write!(formatter, "{}ms", duration.as_millis()),
            Self::Forever => formatter.write_str("forever"),
        }
    }
}

#[cfg(unix)]
pub(crate) struct StateRootLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl StateRootLock {
    /// Takes the lock or fails at once when another process holds it.
    pub(crate) fn acquire(state_root: &Path) -> Result<Self> {
        match Self::try_acquire(state_root)? {
            Some(lock) => Ok(lock),
            None => anyhow::bail!(
                "state root {} is already locked by another daemon{}",
                state_root.display(),
                lock_holder_suffix(state_root)
            ),
        }
    }

    /// Takes the lock, waiting for it as `wait` allows, polling with backoff
    /// and logging while another daemon holds it.
    pub(crate) async fn acquire_waiting(state_root: &Path, wait: StateLockWait) -> Result<Self> {
        if wait == StateLockWait::Never {
            return Self::acquire(state_root);
        }
        let started = Instant::now();
        let mut delay = STATE_LOCK_POLL_INITIAL;
        let mut last_log: Option<Instant> = None;
        loop {
            if let Some(lock) = Self::try_acquire(state_root)? {
                if last_log.is_some() {
                    tracing::info!(
                        state_root = %state_root.display(),
                        waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "acquired the state root lock; this daemon is now active"
                    );
                }
                return Ok(lock);
            }
            if let StateLockWait::For(limit) = wait
                && started.elapsed() >= limit
            {
                anyhow::bail!(
                    "state root {} is still locked by another daemon{} after waiting {}ms",
                    state_root.display(),
                    lock_holder_suffix(state_root),
                    limit.as_millis()
                );
            }
            if last_log.is_none_or(|logged| logged.elapsed() >= STATE_LOCK_WAIT_LOG_INTERVAL) {
                tracing::info!(
                    state_root = %state_root.display(),
                    holder = %lock_holder_pid(state_root).unwrap_or_else(|| "unknown".to_string()),
                    waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    wait = %wait,
                    "state root is locked by another daemon; waiting as a standby"
                );
                last_log = Some(Instant::now());
            }
            let mut sleep_for = delay;
            if let StateLockWait::For(limit) = wait {
                sleep_for = sleep_for.min(limit.saturating_sub(started.elapsed()));
            }
            tokio::time::sleep(sleep_for.max(Duration::from_millis(1))).await;
            delay = (delay * 2).min(STATE_LOCK_POLL_MAX);
        }
    }

    /// Takes the lock if it is free. `Ok(None)` means another process holds it.
    fn try_acquire(state_root: &Path) -> Result<Option<Self>> {
        use std::io::{Seek, Write};
        use std::os::fd::AsRawFd;

        std::fs::create_dir_all(state_root)
            .with_context(|| format!("failed to create state root {}", state_root.display()))?;
        let path = state_root.join("daemon.lock");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open daemon lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("failed to lock state root {}", state_root.display()));
        }
        file.set_len(0)
            .and_then(|()| file.rewind())
            .and_then(|()| writeln!(&file, "pid={}\nmechanism=flock", std::process::id()))
            .with_context(|| format!("failed to write daemon lock {}", path.display()))?;
        Ok(Some(Self { file }))
    }
}

#[cfg(unix)]
impl Drop for StateRootLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Reads the pid the current lock holder wrote, for operator logs.
fn lock_holder_pid(state_root: &Path) -> Option<String> {
    std::fs::read_to_string(state_root.join("daemon.lock"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("pid=").map(str::trim).map(str::to_string))
        .filter(|pid| !pid.is_empty())
}

fn lock_holder_suffix(state_root: &Path) -> String {
    lock_holder_pid(state_root)
        .map(|pid| format!(" (pid {pid})"))
        .unwrap_or_default()
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

    pub(crate) async fn acquire_waiting(state_root: &Path, _wait: StateLockWait) -> Result<Self> {
        Self::acquire(state_root)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn state_lock_wait_parses_durations_and_forever() {
        assert_eq!("forever".parse(), Ok(StateLockWait::Forever));
        assert_eq!("0".parse(), Ok(StateLockWait::Never));
        assert_eq!(
            "1500".parse(),
            Ok(StateLockWait::For(Duration::from_millis(1_500)))
        );
        assert_eq!(
            "250ms".parse(),
            Ok(StateLockWait::For(Duration::from_millis(250)))
        );
        assert_eq!(
            "30s".parse(),
            Ok(StateLockWait::For(Duration::from_secs(30)))
        );
        assert_eq!(
            "5m".parse(),
            Ok(StateLockWait::For(Duration::from_secs(300)))
        );
        assert_eq!(
            "1h".parse(),
            Ok(StateLockWait::For(Duration::from_secs(3_600)))
        );
        assert!("10d".parse::<StateLockWait>().is_err());
        assert!("soon".parse::<StateLockWait>().is_err());
        assert_eq!(StateLockWait::default(), StateLockWait::Never);
    }

    #[tokio::test]
    async fn a_waiting_daemon_takes_the_lock_when_the_holder_releases_it() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let active = StateRootLock::acquire(temp.path())?;
        let holder = std::fs::read_to_string(temp.path().join("daemon.lock"))?;
        assert!(holder.starts_with(&format!("pid={}", std::process::id())));

        // Without a wait, a second daemon still fails at once.
        let error = StateRootLock::acquire(temp.path())
            .err()
            .expect("the lock is held");
        assert!(
            error
                .to_string()
                .contains("already locked by another daemon")
        );

        // A bounded wait gives up with a clear error.
        let started = Instant::now();
        let error = StateRootLock::acquire_waiting(
            temp.path(),
            StateLockWait::For(Duration::from_millis(300)),
        )
        .await
        .err()
        .expect("the lock stays held");
        assert!(error.to_string().contains("after waiting 300ms"), "{error}");
        assert!(started.elapsed() >= Duration::from_millis(300));

        // A standby takes over as soon as the active daemon lets go.
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            drop(active);
        });
        let standby = StateRootLock::acquire_waiting(temp.path(), StateLockWait::Forever).await?;
        release.await?;
        let holder = std::fs::read_to_string(temp.path().join("daemon.lock"))?;
        assert_eq!(
            holder,
            format!("pid={}\nmechanism=flock\n", std::process::id())
        );
        drop(standby);
        Ok(())
    }
}
