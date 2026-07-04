//! Daemon-managed MCP server overlay.
//!
//! Servers added through the runtime API persist here — one JSON document in
//! the state root — and reconnect at every boot alongside the operator-owned
//! `--mcp-config` file, which this overlay never touches. Entries keep the
//! exact Codex-compatible shape the API accepted, so boot-time resolution
//! goes through the same code path as the config file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kheish_mcp::CodexServerConfig;
use kheish_session::write_json_pretty_atomically;
use tokio::sync::Mutex;

const OVERLAY_FILE: &str = "mcp-overlay.json";

/// Serialized overlay document.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct OverlayDocument {
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, CodexServerConfig>,
}

/// Loads and persists runtime-added MCP server entries.
///
/// A single mutation lock serializes add/remove so the in-memory manager and
/// the on-disk document cannot diverge under concurrent API calls.
pub(crate) struct McpOverlayService {
    path: PathBuf,
    mutation: Mutex<()>,
}

impl McpOverlayService {
    pub(crate) fn new(state_root: &Path) -> Self {
        Self {
            path: state_root.join(OVERLAY_FILE),
            mutation: Mutex::new(()),
        }
    }

    /// Serializes one overlay mutation; hold the guard across manager + disk.
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutation.lock().await
    }

    /// Reads the persisted entries; a missing file is an empty overlay and a
    /// corrupt file is quarantined instead of blocking the daemon.
    pub(crate) fn entries(&self) -> BTreeMap<String, CodexServerConfig> {
        let raw = match std::fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return BTreeMap::new();
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "failed to read MCP overlay; treating it as empty"
                );
                return BTreeMap::new();
            }
        };
        match serde_json::from_slice::<OverlayDocument>(&raw) {
            Ok(document) => document.servers,
            Err(error) => {
                let quarantine = self.path.with_extension("json.corrupt");
                tracing::warn!(
                    path = %self.path.display(),
                    quarantine = %quarantine.display(),
                    error = %error,
                    "corrupt MCP overlay quarantined; starting with an empty overlay"
                );
                let _ = std::fs::rename(&self.path, &quarantine);
                BTreeMap::new()
            }
        }
    }

    /// Persists the full entry map atomically.
    pub(crate) fn save(&self, servers: &BTreeMap<String, CodexServerConfig>) -> Result<()> {
        write_json_pretty_atomically(
            &self.path,
            &OverlayDocument {
                version: 1,
                servers: servers.clone(),
            },
        )
        .with_context(|| format!("failed to persist MCP overlay {}", self.path.display()))
    }
}
