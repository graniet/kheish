//! Daemon-managed model-route overlay.
//!
//! Routes added through the runtime API persist here — one JSON document in the
//! state root — and rebuild at every boot alongside the operator's routes file,
//! which this overlay never touches. The list is insertion-ordered so the route
//! that was the default when the inventory was empty stays first (and default)
//! across restarts.
//!
//! API keys never land in this document: they live in the encrypted secret
//! store under `routes.<route_id>.api_key`, and the overlay entry only records
//! enough to rebuild the driver by resolving that slot at boot.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kheish_session::write_json_pretty_atomically;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const OVERLAY_FILE: &str = "routes-overlay.json";

/// One persisted runtime-added route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteOverlayEntry {
    pub route_id: String,
    pub provider: String,
    pub model: String,
    /// Secret-store slot holding this route's API key.
    pub api_key_secret_ref: String,
}

/// Serialized overlay document.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OverlayDocument {
    version: u32,
    #[serde(default)]
    routes: Vec<RouteOverlayEntry>,
}

/// Loads and persists runtime-added model-route entries.
///
/// A single mutation lock serializes add/remove so the in-memory route registry
/// and the on-disk document cannot diverge under concurrent API calls.
pub(crate) struct RoutesOverlayService {
    path: PathBuf,
    mutation: Mutex<()>,
}

impl RoutesOverlayService {
    pub(crate) fn new(state_root: &Path) -> Self {
        Self {
            path: state_root.join(OVERLAY_FILE),
            mutation: Mutex::new(()),
        }
    }

    /// Serializes one overlay mutation; hold the guard across registry + disk.
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutation.lock().await
    }

    /// Reads the persisted entries; a missing file is an empty overlay and a
    /// corrupt file is quarantined instead of blocking the daemon.
    pub(crate) fn entries(&self) -> Vec<RouteOverlayEntry> {
        let raw = match std::fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Vec::new();
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "failed to read routes overlay; treating it as empty"
                );
                return Vec::new();
            }
        };
        match serde_json::from_slice::<OverlayDocument>(&raw) {
            Ok(document) => document.routes,
            Err(error) => {
                let quarantine = self.path.with_extension("json.corrupt");
                tracing::warn!(
                    path = %self.path.display(),
                    quarantine = %quarantine.display(),
                    error = %error,
                    "corrupt routes overlay quarantined; starting with an empty overlay"
                );
                let _ = std::fs::rename(&self.path, &quarantine);
                Vec::new()
            }
        }
    }

    /// Persists the full route list atomically.
    pub(crate) fn save(&self, routes: &[RouteOverlayEntry]) -> Result<()> {
        write_json_pretty_atomically(
            &self.path,
            &OverlayDocument {
                version: 1,
                routes: routes.to_vec(),
            },
        )
        .with_context(|| format!("failed to persist routes overlay {}", self.path.display()))
    }
}
