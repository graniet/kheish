use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use tokio::sync::Mutex;

use crate::derivations::{DerivationStatus, FileDerivationStore, StoredDerivationRecord};

/// Owns durable derivation records together with in-memory cache-key deduplication.
pub(crate) struct DerivationService {
    store: FileDerivationStore,
    records: Mutex<BTreeMap<String, StoredDerivationRecord>>,
    cache: Mutex<BTreeMap<String, String>>,
    creation_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    next_derivation_id: AtomicU64,
}

impl DerivationService {
    /// Creates a new derivation service backed by one file store.
    pub(crate) fn new(
        store: FileDerivationStore,
        records: BTreeMap<String, StoredDerivationRecord>,
        next_derivation_id: AtomicU64,
    ) -> Self {
        let cache = build_derivation_cache_index(&records);
        Self {
            store,
            records: Mutex::new(records),
            cache: Mutex::new(cache),
            creation_locks: Mutex::new(BTreeMap::new()),
            next_derivation_id,
        }
    }

    /// Returns one named single-flight lock.
    pub(crate) async fn creation_lock_for_key(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.creation_locks.lock().await;
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Returns the single-flight lock for one cache key.
    pub(crate) async fn creation_lock_for_cache_key(&self, cache_key: &str) -> Arc<Mutex<()>> {
        self.creation_lock_for_key(cache_key).await
    }

    /// Returns one fresh daemon-managed derivation identifier.
    pub(crate) fn next_derivation_id(&self) -> String {
        format!(
            "derivation-{}",
            self.next_derivation_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Returns every known derivation optionally filtered by a free-text query.
    pub(crate) async fn list(&self, query: Option<&str>) -> Vec<StoredDerivationRecord> {
        let normalized = query
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        self.records
            .lock()
            .await
            .values()
            .filter(|record| {
                normalized.as_ref().is_none_or(|query| {
                    record.derivation_id.to_ascii_lowercase().contains(query)
                        || record.profile.as_str().contains(query)
                        || record
                            .subject
                            .stable_key()
                            .to_ascii_lowercase()
                            .contains(query)
                        || record
                            .source_fingerprint
                            .to_ascii_lowercase()
                            .contains(query)
                        || record.status.as_str().contains(query)
                        || record
                            .error
                            .as_ref()
                            .is_some_and(|error| error.to_ascii_lowercase().contains(query))
                        || record.result_asset_id.to_ascii_lowercase().contains(query)
                        || record
                            .backend
                            .as_ref()
                            .and_then(|backend| backend.timestamp_asset_id.as_ref())
                            .is_some_and(|asset_id| asset_id.to_ascii_lowercase().contains(query))
                })
            })
            .cloned()
            .collect()
    }

    /// Returns one derivation by identifier when it exists.
    pub(crate) async fn get(&self, derivation_id: &str) -> Option<StoredDerivationRecord> {
        self.records.lock().await.get(derivation_id).cloned()
    }

    /// Returns one derivation by cache key when it already exists.
    pub(crate) async fn get_by_cache_key(&self, cache_key: &str) -> Option<StoredDerivationRecord> {
        let derivation_id = self.cache.lock().await.get(cache_key).cloned()?;
        self.records.lock().await.get(&derivation_id).cloned()
    }

    /// Persists one new derivation record and registers it in the in-memory caches.
    pub(crate) async fn create(
        &self,
        record: StoredDerivationRecord,
    ) -> Result<StoredDerivationRecord> {
        let cache_key = record.cache_key();
        let existing_derivation_id = { self.cache.lock().await.get(&cache_key).cloned() };
        let should_replace_cache = if let Some(existing_derivation_id) = existing_derivation_id {
            let records = self.records.lock().await;
            records
                .get(&existing_derivation_id)
                .is_none_or(|existing| should_replace_cached_derivation(existing, &record))
        } else {
            true
        };
        self.store.save_derivation(&record)?;
        if should_replace_cache {
            self.cache
                .lock()
                .await
                .insert(cache_key, record.derivation_id.clone());
        }
        self.records
            .lock()
            .await
            .insert(record.derivation_id.clone(), record.clone());
        Ok(record)
    }
}

fn build_derivation_cache_index(
    records: &BTreeMap<String, StoredDerivationRecord>,
) -> BTreeMap<String, String> {
    let mut cache = BTreeMap::new();
    for record in records.values() {
        let cache_key = record.cache_key();
        let should_replace = cache
            .get(&cache_key)
            .and_then(|derivation_id| records.get(derivation_id))
            .is_none_or(|existing| should_replace_cached_derivation(existing, record));
        if should_replace {
            cache.insert(cache_key, record.derivation_id.clone());
        }
    }
    cache
}

fn should_replace_cached_derivation(
    existing: &StoredDerivationRecord,
    candidate: &StoredDerivationRecord,
) -> bool {
    match (&existing.status, &candidate.status) {
        (DerivationStatus::Failed, DerivationStatus::Completed) => return true,
        (DerivationStatus::Completed, DerivationStatus::Failed) => return false,
        _ => {}
    }
    candidate.created_at_ms > existing.created_at_ms
        || (candidate.created_at_ms == existing.created_at_ms
            && derivation_numeric_suffix(&candidate.derivation_id)
                > derivation_numeric_suffix(&existing.derivation_id))
}

fn derivation_numeric_suffix(derivation_id: &str) -> Option<u64> {
    derivation_id
        .strip_prefix("derivation-")
        .and_then(|value| value.parse().ok())
}
