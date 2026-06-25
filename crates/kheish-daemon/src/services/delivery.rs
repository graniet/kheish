use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use kheish_output::{OutputHost, OutputManifest, ResponseEnvelope};

use crate::DaemonOutputRecord;
use crate::delivery::{
    DeliveryBackpressureResetResponse, DeliveryBulkReplayResponse, DeliveryListFilter,
    DeliveryQueue, DeliveryQueueStatusView, DeliveryReplayResponse, DeliveryStatus, DeliveryView,
    PendingDeliveryRecord,
};
use crate::state::FileDaemonStore;

/// Owns persisted daemon outputs, the in-memory output cache, and output delivery helpers.
pub(crate) struct DeliveryService {
    store: FileDaemonStore,
    output_cache: Mutex<BTreeMap<String, Vec<DaemonOutputRecord>>>,
    output_host: Arc<OutputHost>,
    queue: Arc<DeliveryQueue>,
}

impl DeliveryService {
    /// Creates a new delivery service backed by the daemon store and output host.
    pub(crate) fn new(
        store: FileDaemonStore,
        output_host: Arc<OutputHost>,
        queue: Arc<DeliveryQueue>,
    ) -> Self {
        Self {
            store,
            output_cache: Mutex::new(BTreeMap::new()),
            output_host,
            queue,
        }
    }

    /// Returns the registered output plugin manifests.
    pub(crate) fn output_manifests(&self) -> Vec<OutputManifest> {
        self.output_host.manifests()
    }

    /// Returns the registered output plugin names.
    pub(crate) fn available_reply_plugins(&self) -> BTreeSet<String> {
        self.output_manifests()
            .into_iter()
            .map(|manifest| manifest.name)
            .collect()
    }

    /// Persists one daemon output record and updates the in-memory cache.
    pub(crate) async fn append_output(&self, output: &DaemonOutputRecord) -> Result<()> {
        self.store.append_output(output)?;
        self.output_cache
            .lock()
            .await
            .entry(output.session_id.clone())
            .or_default()
            .push(output.clone());
        Ok(())
    }

    /// Returns the cached outputs currently held in memory for one session.
    pub(crate) async fn cached_outputs(&self, session_id: &str) -> Vec<DaemonOutputRecord> {
        self.output_cache
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the merged persisted and in-memory outputs for one session.
    pub(crate) async fn session_outputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<DaemonOutputRecord>> {
        let in_memory = self.cached_outputs(session_id).await;
        self.merge_session_outputs(session_id, in_memory).await
    }

    /// Merges persisted outputs with additional in-memory records without duplication.
    pub(crate) async fn merge_session_outputs(
        &self,
        session_id: &str,
        in_memory: Vec<DaemonOutputRecord>,
    ) -> Result<Vec<DaemonOutputRecord>> {
        let mut outputs = self.store.load_outputs(session_id)?;
        for output in in_memory {
            if outputs.contains(&output) {
                continue;
            }
            outputs.push(output);
        }
        Ok(outputs)
    }

    /// Delivers one response envelope through the configured output host.
    pub(crate) async fn deliver(&self, envelope: ResponseEnvelope) -> Result<()> {
        self.output_host.deliver(envelope).await
    }

    /// Returns redacted operator views for queued, delivered, and dead-lettered deliveries.
    pub(crate) async fn list_deliveries(
        &self,
        filter: DeliveryListFilter,
    ) -> Result<Vec<DeliveryView>> {
        self.queue.list_views(filter).await
    }

    /// Returns one redacted operator delivery view.
    pub(crate) async fn get_delivery(&self, delivery_id: &str) -> Result<Option<DeliveryView>> {
        self.queue.get_view(delivery_id).await
    }

    /// Returns unredacted delivery records for internal reference-graph accounting.
    pub(crate) async fn reference_records(
        &self,
    ) -> Result<Vec<(DeliveryStatus, PendingDeliveryRecord)>> {
        self.queue.reference_records().await
    }

    /// Returns cheap queue and worker-liveness counters for `/v1/status`.
    pub(crate) async fn status_snapshot(&self, now: u64) -> Result<DeliveryQueueStatusView> {
        self.queue.status_snapshot(now).await
    }

    /// Replays one dead-lettered delivery by creating a new pending queue item.
    pub(crate) async fn replay_dead_letter(
        &self,
        delivery_id: &str,
        force: bool,
    ) -> Result<Option<DeliveryReplayResponse>> {
        self.queue.replay_dead_letter(delivery_id, force).await
    }

    /// Marks one dead-lettered delivery as operator-resolved without deleting the audit ledger.
    pub(crate) async fn resolve_dead_letter(
        &self,
        delivery_id: &str,
        reason: &str,
    ) -> Result<Option<DeliveryView>> {
        self.queue.resolve_dead_letter(delivery_id, reason).await
    }

    /// Replays a bounded batch of dead-lettered deliveries using server-side filtering.
    pub(crate) async fn bulk_replay_dead_letters(
        &self,
        filter: DeliveryListFilter,
        force: bool,
        dry_run: bool,
        unresolved_only: bool,
        limit: Option<usize>,
    ) -> Result<DeliveryBulkReplayResponse> {
        self.queue
            .bulk_replay_dead_letters(filter, force, dry_run, unresolved_only, limit)
            .await
    }

    /// Resets persisted delivery target backpressure by redacted target digest and/or plugin.
    pub(crate) async fn reset_backpressure(
        &self,
        target: Option<&str>,
        plugin: Option<&str>,
        dry_run: bool,
    ) -> Result<DeliveryBackpressureResetResponse> {
        self.queue
            .reset_target_backpressure(target, plugin, dry_run)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;

    use super::DeliveryService;
    use crate::DaemonOutputRecord;
    use crate::delivery::{DeliveryDispatcher, DeliveryQueue};
    use crate::state::FileDaemonStore;
    use kheish_output::{OutputHost, OutputManifest, OutputPlugin, ResponseEnvelope};
    use kheish_types::{ConversationKey, ReplyHandle};

    #[derive(Default)]
    struct CapturingOutputPlugin {
        deliveries: Arc<StdMutex<Vec<ResponseEnvelope>>>,
    }

    #[async_trait]
    impl OutputPlugin for CapturingOutputPlugin {
        fn manifest(&self) -> OutputManifest {
            OutputManifest {
                name: "capture".to_string(),
                version: "1".to_string(),
                description: "Captures output deliveries for tests".to_string(),
            }
        }

        async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
            self.deliveries
                .lock()
                .expect("test output delivery mutex poisoned")
                .push(response);
            Ok(())
        }
    }

    fn sample_output(content: &str) -> DaemonOutputRecord {
        DaemonOutputRecord {
            session_id: "session-1".to_string(),
            run_id: Some("run-1".to_string()),
            content: content.to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            source_kind: None,
            plugin: Some("daemon".to_string()),
            address: Some("session-1".to_string()),
        }
    }

    fn test_queue(temp: &tempfile::TempDir) -> Result<Arc<DeliveryQueue>> {
        Ok(Arc::new(DeliveryQueue::load(
            temp.path().join("deliveries.json"),
            Arc::new(DeliveryDispatcher::new()),
        )?))
    }

    #[tokio::test]
    async fn delivery_service_round_trips_outputs_without_duplicates() -> Result<()> {
        let temp = tempdir()?;
        let service = DeliveryService::new(
            FileDaemonStore::new(temp.path()),
            Arc::new(OutputHost::new()),
            test_queue(&temp)?,
        );

        service.append_output(&sample_output("hello")).await?;

        let outputs = service.session_outputs("session-1").await?;
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].content, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn delivery_service_exposes_registered_reply_plugins() -> Result<()> {
        let temp = tempdir()?;
        let mut host = OutputHost::new();
        host.register(CapturingOutputPlugin::default());
        let service = DeliveryService::new(
            FileDaemonStore::new(temp.path()),
            Arc::new(host),
            test_queue(&temp)?,
        );

        assert!(service.available_reply_plugins().contains("capture"));
        Ok(())
    }

    #[tokio::test]
    async fn delivery_service_delivers_through_the_output_host() -> Result<()> {
        let temp = tempdir()?;
        let deliveries = Arc::new(StdMutex::new(Vec::new()));
        let plugin = CapturingOutputPlugin {
            deliveries: deliveries.clone(),
        };
        let mut host = OutputHost::new();
        host.register(plugin);
        let service = DeliveryService::new(
            FileDaemonStore::new(temp.path()),
            Arc::new(host),
            test_queue(&temp)?,
        );

        service
            .deliver(ResponseEnvelope {
                conversation: ConversationKey {
                    session_id: "session-1".to_string(),
                    thread_id: None,
                },
                reply_targets: vec![ReplyHandle {
                    plugin: "capture".to_string(),
                    address: "addr-1".to_string(),
                }],
                reply: Some(ReplyHandle {
                    plugin: "capture".to_string(),
                    address: "addr-1".to_string(),
                }),
                content: "hello".to_string(),
                parts: Vec::new(),
                artifacts: Vec::new(),
                metadata: serde_json::Value::Null,
            })
            .await?;

        let deliveries = deliveries
            .lock()
            .expect("test output delivery mutex poisoned");
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].content, "hello");
        Ok(())
    }
}
