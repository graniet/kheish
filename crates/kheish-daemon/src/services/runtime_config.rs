//! Durable daemon runtime-configuration revisions.

use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kheish_session::write_json_pretty_atomically;
use serde::{Deserialize, Serialize};
use tokio::sync::{
    Mutex, MutexGuard, RwLock as AsyncRwLock, RwLockReadGuard as AsyncRwLockReadGuard,
    RwLockWriteGuard as AsyncRwLockWriteGuard,
};

use crate::state_files::read_json_or_quarantine;
use crate::{
    RuntimeConfigMetadataView, RuntimeConfigRevisionListResponse, RuntimeConfigRevisionView,
    problems::DaemonProblem,
};

const RUNTIME_CONFIG_FILENAME: &str = "runtime-config.json";
const RUNTIME_CONFIG_HISTORY_LIMIT: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct RuntimeConfigDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current: Option<RuntimeConfigRevisionView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<RuntimeConfigRevisionView>,
}

#[derive(Clone, Debug)]
struct FileRuntimeConfigStore {
    path: PathBuf,
}

impl FileRuntimeConfigStore {
    fn new(root: impl AsRef<Path>) -> Self {
        Self {
            path: root.as_ref().join(RUNTIME_CONFIG_FILENAME),
        }
    }

    fn load(&self) -> Result<RuntimeConfigDocument> {
        Ok(read_json_or_quarantine(&self.path, "runtime config")?.unwrap_or_default())
    }

    fn save(&self, document: &RuntimeConfigDocument) -> Result<()> {
        write_json_pretty_atomically(&self.path, document)
    }
}

/// Serializes daemon-owned runtime mutations and keeps a rollbackable revision history.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeConfigService {
    store: FileRuntimeConfigStore,
    document: Arc<RwLock<RuntimeConfigDocument>>,
    mutation_lock: Arc<Mutex<()>>,
    visibility_lock: Arc<AsyncRwLock<()>>,
}

impl RuntimeConfigService {
    pub(crate) fn new(state_root: impl AsRef<Path>) -> Result<Self> {
        let store = FileRuntimeConfigStore::new(state_root);
        let document = store.load()?;
        validate_document(&document)?;
        Ok(Self {
            store,
            document: Arc::new(RwLock::new(document)),
            mutation_lock: Arc::new(Mutex::new(())),
            visibility_lock: Arc::new(AsyncRwLock::new(())),
        })
    }

    pub(crate) async fn mutation_guard(&self) -> MutexGuard<'_, ()> {
        self.mutation_lock.lock().await
    }

    pub(crate) async fn visibility_guard(&self) -> AsyncRwLockWriteGuard<'_, ()> {
        self.visibility_lock.write().await
    }

    pub(crate) async fn snapshot_guard(&self) -> AsyncRwLockReadGuard<'_, ()> {
        self.visibility_lock.read().await
    }

    pub(crate) fn current(&self) -> Option<RuntimeConfigRevisionView> {
        self.document.read().current.clone()
    }

    pub(crate) fn metadata(&self) -> RuntimeConfigMetadataView {
        let document = self.document.read();
        RuntimeConfigMetadataView {
            revision: document
                .current
                .as_ref()
                .map(|revision| revision.revision)
                .unwrap_or_default(),
            updated_at_ms: document
                .current
                .as_ref()
                .map(|revision| revision.updated_at_ms),
            persisted: document.current.is_some(),
            history_len: document.history.len(),
            history_limit: RUNTIME_CONFIG_HISTORY_LIMIT,
            store_path: Some(self.store.path.display().to_string()),
        }
    }

    pub(crate) fn list_revisions(&self) -> RuntimeConfigRevisionListResponse {
        let document = self.document.read();
        let mut revisions = document.history.clone();
        if let Some(current) = document.current.clone() {
            revisions.push(current);
        }
        revisions.sort_by(|left, right| right.revision.cmp(&left.revision));
        RuntimeConfigRevisionListResponse { revisions }
    }

    pub(crate) fn require_expected_revision(&self, expected: Option<u64>) -> Result<()> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let current = self
            .document
            .read()
            .current
            .as_ref()
            .map(|revision| revision.revision)
            .unwrap_or_default();
        if current != expected {
            return Err(DaemonProblem::runtime_revision_conflict(format!(
                "runtime config revision conflict: expected {expected}, current {current}"
            ))
            .into());
        }
        Ok(())
    }

    pub(crate) fn previous_revision(&self) -> Result<RuntimeConfigRevisionView> {
        let document = self.document.read();
        document.history.last().cloned().ok_or_else(|| {
            DaemonProblem::runtime_revision_not_found(
                "runtime config has no previous revision to roll back to",
            )
            .into()
        })
    }

    pub(crate) fn revision(&self, revision_id: u64) -> Result<RuntimeConfigRevisionView> {
        let document = self.document.read();
        if let Some(current) = &document.current
            && current.revision == revision_id
        {
            return Ok(current.clone());
        }
        document
            .history
            .iter()
            .find(|revision| revision.revision == revision_id)
            .cloned()
            .ok_or_else(|| {
                DaemonProblem::runtime_revision_not_found(format!(
                    "unknown runtime config revision {revision_id}"
                ))
                .into()
            })
    }

    pub(crate) fn append_revision(
        &self,
        mut revision: RuntimeConfigRevisionView,
    ) -> Result<RuntimeConfigRevisionView> {
        let current_document = self.document.read().clone();
        let mut next_document = current_document;
        revision.revision = next_document
            .current
            .as_ref()
            .map(|current| current.revision.saturating_add(1))
            .unwrap_or(1);
        revision.updated_at_ms = crate::now_ms();
        if let Some(current) = next_document.current.replace(revision.clone()) {
            next_document.history.push(current);
        }
        if next_document.history.len() > RUNTIME_CONFIG_HISTORY_LIMIT {
            let excess = next_document.history.len() - RUNTIME_CONFIG_HISTORY_LIMIT;
            next_document.history.drain(0..excess);
        }
        validate_document(&next_document)?;
        self.store.save(&next_document)?;
        *self.document.write() = next_document;
        Ok(revision)
    }
}

fn validate_document(document: &RuntimeConfigDocument) -> Result<()> {
    let mut last_revision = 0;
    for revision in &document.history {
        if revision.revision == 0 {
            bail!("runtime config history contains revision 0");
        }
        if revision.revision <= last_revision {
            bail!("runtime config history revisions must be strictly increasing");
        }
        revision.run_memory_policy.validate().with_context(|| {
            format!(
                "invalid run-memory policy in revision {}",
                revision.revision
            )
        })?;
        revision.tool_runtime_limits.validate().with_context(|| {
            format!(
                "invalid tool-runtime limits in revision {}",
                revision.revision
            )
        })?;
        last_revision = revision.revision;
    }
    if let Some(current) = &document.current {
        if current.revision == 0 {
            bail!("runtime config current revision must be greater than zero");
        }
        if current.revision <= last_revision {
            bail!("runtime config current revision must follow history");
        }
        current.run_memory_policy.validate().with_context(|| {
            format!("invalid run-memory policy in revision {}", current.revision)
        })?;
        current.tool_runtime_limits.validate().with_context(|| {
            format!(
                "invalid tool-runtime limits in revision {}",
                current.revision
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kheish_runtime::{
        DebugCaptureLevel, PermissionMode, SystemPromptSettings, ToolRuntimeLimits,
    };
    use kheish_types::HookSettings;
    use tempfile::tempdir;

    fn revision(setting: &str) -> RuntimeConfigRevisionView {
        RuntimeConfigRevisionView {
            revision: 0,
            updated_at_ms: 0,
            source: "test".to_string(),
            setting: setting.to_string(),
            rollback_of_revision: None,
            route_id: Some("openai".to_string()),
            provider: Some("openai".to_string()),
            model: Some("gpt-5.4".to_string()),
            permission_mode: PermissionMode::Default,
            system_prompt: SystemPromptSettings::default(),
            hooks: HookSettings::default(),
            debug_level: DebugCaptureLevel::Off,
            learning_policy: None,
            run_memory_policy: crate::RunMemoryPolicyConfig::default(),
            tool_runtime_limits: ToolRuntimeLimits::default(),
        }
    }

    #[test]
    fn runtime_config_service_assigns_monotonic_revisions_and_reloads() -> Result<()> {
        let temp = tempdir()?;
        let service = RuntimeConfigService::new(temp.path())?;
        let first = service.append_revision(revision("permission_mode"))?;
        let second = service.append_revision(revision("debug_level"))?;

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        assert_eq!(service.metadata().revision, 2);
        assert_eq!(service.metadata().history_len, 1);

        let reloaded = RuntimeConfigService::new(temp.path())?;
        assert_eq!(reloaded.metadata().revision, 2);
        assert_eq!(reloaded.previous_revision()?.revision, 1);
        Ok(())
    }

    #[test]
    fn runtime_config_service_rejects_stale_expected_revision() -> Result<()> {
        let temp = tempdir()?;
        let service = RuntimeConfigService::new(temp.path())?;
        service.append_revision(revision("permission_mode"))?;

        service.require_expected_revision(Some(1))?;
        let error = service
            .require_expected_revision(Some(0))
            .expect_err("stale revision should fail");
        assert!(
            error
                .to_string()
                .contains("runtime config revision conflict")
        );
        Ok(())
    }
}
