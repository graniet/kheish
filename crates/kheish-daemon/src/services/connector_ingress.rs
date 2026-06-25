use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

use kheish_session::{
    prepare_storage_path_for_write, resolve_storage_path_for_read, write_json_pretty_atomically,
};

use crate::connectors::ConnectorKind;
use crate::now_ms;
use crate::state::{
    ConnectorCursorState, ConnectorIngressLookup, ConnectorIngressReceiptState,
    ConnectorIngressReservation, SessionIndex,
};
use crate::state_files::read_json_or_quarantine;

const MAX_CONNECTOR_INGRESS_PENDING_AGE_MS: u64 = 15 * 60 * 1000;
const MAX_CONNECTOR_INGRESS_RECEIPT_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_CONNECTOR_INGRESS_RECEIPTS_PER_CONNECTOR: usize = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ConnectorIngressShard {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<ConnectorCursorState>,
    #[serde(default)]
    receipts: BTreeMap<String, ConnectorIngressReceiptState>,
}

impl ConnectorIngressShard {
    fn is_empty(&self) -> bool {
        self.cursor.is_none() && self.receipts.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ConnectorShardKey {
    kind: ConnectorKind,
    name: String,
}

impl ConnectorShardKey {
    fn new(kind: ConnectorKind, name: &str) -> Self {
        Self {
            kind,
            name: name.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct FileConnectorIngressStore {
    root: PathBuf,
}

impl FileConnectorIngressStore {
    fn new(state_root: &Path) -> Self {
        Self {
            root: state_root.join("connector-ingress"),
        }
    }

    fn shard_root(&self, kind: ConnectorKind) -> PathBuf {
        self.root.join(kind.as_str())
    }

    fn shard_path_for_read(&self, key: &ConnectorShardKey) -> PathBuf {
        resolve_storage_path_for_read(&self.shard_root(key.kind), &key.name, "json")
    }

    fn shard_path_for_write(&self, key: &ConnectorShardKey) -> Result<PathBuf> {
        prepare_storage_path_for_write(&self.shard_root(key.kind), &key.name, "json")
    }

    fn load_shard(&self, key: &ConnectorShardKey) -> Result<ConnectorIngressShard> {
        Ok(
            read_json_or_quarantine(&self.shard_path_for_read(key), "connector ingress shard")?
                .unwrap_or_default(),
        )
    }

    fn save_shard(&self, key: &ConnectorShardKey, shard: &ConnectorIngressShard) -> Result<()> {
        let path = self.shard_path_for_write(key)?;
        if shard.is_empty() {
            match fs::remove_file(path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        write_json_pretty_atomically(&path, shard)
    }
}

/// Persists connector ingress receipts and cursors in per-connector shard files.
pub(crate) struct ConnectorIngressService {
    store: FileConnectorIngressStore,
    shards: Mutex<BTreeMap<ConnectorShardKey, ConnectorIngressShard>>,
}

impl ConnectorIngressService {
    /// Loads the per-connector ingress state store rooted under the daemon state root.
    pub(crate) fn load(state_root: &Path) -> Self {
        Self {
            store: FileConnectorIngressStore::new(state_root),
            shards: Mutex::new(BTreeMap::new()),
        }
    }

    async fn update_shard<T>(
        &self,
        kind: ConnectorKind,
        name: &str,
        update: impl FnOnce(&mut ConnectorIngressShard) -> Result<(T, bool)>,
    ) -> Result<T> {
        let key = ConnectorShardKey::new(kind, name);
        let mut shards = self.shards.lock().await;
        let mut shard = match shards.get(&key) {
            Some(shard) => shard.clone(),
            None => {
                let loaded = self.store.load_shard(&key)?;
                shards.insert(key.clone(), loaded.clone());
                loaded
            }
        };
        let (result, changed) = update(&mut shard)?;
        if changed {
            self.store.save_shard(&key, &shard)?;
            shards.insert(key, shard);
        }
        Ok(result)
    }

    /// Resolves one legacy raw cursor key and returns the stored next update identifier.
    pub(crate) async fn connector_next_update_id_from_legacy_key(&self, key: &str) -> Option<i64> {
        let shard_key = parse_legacy_polling_cursor_key(key)?;
        self.connector_next_update_id(shard_key.kind, &shard_key.name)
            .await
    }

    /// Resolves one legacy raw cursor key and persists the next update identifier.
    pub(crate) async fn remember_connector_next_update_id_from_legacy_key(
        &self,
        key: &str,
        next_update_id: i64,
    ) -> Result<()> {
        let shard_key = parse_legacy_polling_cursor_key(key)
            .ok_or_else(|| anyhow!("unrecognized connector cursor key {key}"))?;
        self.remember_connector_next_update_id(shard_key.kind, &shard_key.name, next_update_id)
            .await
    }

    /// Resolves one legacy raw ingress receipt key and reserves it for submission.
    pub(crate) async fn begin_connector_ingress_from_legacy_key(
        &self,
        key: &str,
    ) -> Result<ConnectorIngressReservation> {
        let (shard_key, receipt_key) = parse_legacy_receipt_key(key)
            .ok_or_else(|| anyhow!("unrecognized connector ingress key {key}"))?;
        self.begin_connector_ingress(shard_key.kind, &shard_key.name, &receipt_key)
            .await
    }

    /// Resolves one legacy raw ingress receipt key and reserves it with a payload fingerprint.
    pub(crate) async fn begin_connector_ingress_with_fingerprint_from_legacy_key(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<ConnectorIngressReservation> {
        let (shard_key, receipt_key) = parse_legacy_receipt_key(key)
            .ok_or_else(|| anyhow!("unrecognized connector ingress key {key}"))?;
        self.begin_connector_ingress_with_fingerprint(
            shard_key.kind,
            &shard_key.name,
            &receipt_key,
            fingerprint,
        )
        .await
    }

    /// Resolves one legacy raw ingress receipt key without reserving a new submission.
    pub(crate) async fn lookup_connector_ingress_from_legacy_key(
        &self,
        key: &str,
    ) -> Result<ConnectorIngressLookup> {
        let (shard_key, receipt_key) = parse_legacy_receipt_key(key)
            .ok_or_else(|| anyhow!("unrecognized connector ingress key {key}"))?;
        self.lookup_connector_ingress(shard_key.kind, &shard_key.name, &receipt_key)
            .await
    }

    /// Resolves one legacy raw ingress receipt key and marks it as submitted.
    pub(crate) async fn remember_connector_ingress_run_from_legacy_key(
        &self,
        key: &str,
        run_id: &str,
    ) -> Result<()> {
        let (shard_key, receipt_key) = parse_legacy_receipt_key(key)
            .ok_or_else(|| anyhow!("unrecognized connector ingress key {key}"))?;
        self.remember_connector_ingress_run(shard_key.kind, &shard_key.name, &receipt_key, run_id)
            .await
    }

    /// Resolves one legacy raw ingress receipt key and clears it from the shard store.
    pub(crate) async fn forget_connector_ingress_from_legacy_key(&self, key: &str) -> Result<()> {
        let (shard_key, receipt_key) = parse_legacy_receipt_key(key)
            .ok_or_else(|| anyhow!("unrecognized connector ingress key {key}"))?;
        self.forget_connector_ingress(shard_key.kind, &shard_key.name, &receipt_key)
            .await
    }

    /// Returns the current polling cursor for one connector when present.
    pub(crate) async fn connector_next_update_id(
        &self,
        kind: ConnectorKind,
        name: &str,
    ) -> Option<i64> {
        self.update_shard(kind, name, |shard| {
            let next_update_id = match shard.cursor.as_ref() {
                Some(ConnectorCursorState::TelegramPolling { next_update_id }) => {
                    Some(*next_update_id)
                }
                None => None,
            };
            Ok((next_update_id, false))
        })
        .await
        .ok()
        .flatten()
    }

    /// Persists the next Telegram polling cursor for one connector.
    pub(crate) async fn remember_connector_next_update_id(
        &self,
        kind: ConnectorKind,
        name: &str,
        next_update_id: i64,
    ) -> Result<()> {
        let cursor = ConnectorCursorState::TelegramPolling { next_update_id };
        self.update_shard(kind, name, |shard| {
            if shard.cursor.as_ref() == Some(&cursor) {
                return Ok(((), false));
            }
            shard.cursor = Some(cursor);
            Ok(((), true))
        })
        .await
    }

    /// Reserves one connector ingress key for idempotent submission.
    pub(crate) async fn begin_connector_ingress(
        &self,
        kind: ConnectorKind,
        name: &str,
        key: &str,
    ) -> Result<ConnectorIngressReservation> {
        self.begin_connector_ingress_inner(kind, name, key, None)
            .await
    }

    /// Reserves one connector ingress key for idempotent submission and binds a fingerprint.
    pub(crate) async fn begin_connector_ingress_with_fingerprint(
        &self,
        kind: ConnectorKind,
        name: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<ConnectorIngressReservation> {
        self.begin_connector_ingress_inner(kind, name, key, Some(fingerprint))
            .await
    }

    async fn begin_connector_ingress_inner(
        &self,
        kind: ConnectorKind,
        name: &str,
        key: &str,
        fingerprint: Option<&str>,
    ) -> Result<ConnectorIngressReservation> {
        let now = now_ms();
        self.update_shard(kind, name, |shard| {
            let mut changed = prune_receipts(kind, name, &mut shard.receipts, now);
            let reservation = match shard.receipts.get(key) {
                Some(ConnectorIngressReceiptState::Pending {
                    fingerprint: existing,
                    ..
                }) => {
                    ensure_compatible_fingerprint(existing.as_deref(), fingerprint)?;
                    ConnectorIngressReservation::Pending
                }
                Some(ConnectorIngressReceiptState::Submitted {
                    run_id,
                    fingerprint: existing,
                    ..
                }) => {
                    ensure_compatible_fingerprint(existing.as_deref(), fingerprint)?;
                    ConnectorIngressReservation::Existing {
                        run_id: run_id.clone(),
                    }
                }
                None => {
                    shard.receipts.insert(
                        key.to_string(),
                        ConnectorIngressReceiptState::Pending {
                            recorded_at_ms: now,
                            fingerprint: fingerprint.map(str::to_string),
                        },
                    );
                    changed = true;
                    ConnectorIngressReservation::Reserved
                }
            };
            Ok((reservation, changed))
        })
        .await
    }

    /// Looks up one connector ingress key without creating a pending receipt.
    pub(crate) async fn lookup_connector_ingress(
        &self,
        kind: ConnectorKind,
        name: &str,
        key: &str,
    ) -> Result<ConnectorIngressLookup> {
        let now = now_ms();
        self.update_shard(kind, name, |shard| {
            let changed = prune_receipts(kind, name, &mut shard.receipts, now);
            let lookup = match shard.receipts.get(key) {
                Some(ConnectorIngressReceiptState::Pending { .. }) => {
                    ConnectorIngressLookup::Pending
                }
                Some(ConnectorIngressReceiptState::Submitted { run_id, .. }) => {
                    ConnectorIngressLookup::Existing {
                        run_id: run_id.clone(),
                    }
                }
                None => ConnectorIngressLookup::Absent,
            };
            Ok((lookup, changed))
        })
        .await
    }

    /// Marks one connector ingress key as submitted for one run identifier.
    pub(crate) async fn remember_connector_ingress_run(
        &self,
        kind: ConnectorKind,
        name: &str,
        key: &str,
        run_id: &str,
    ) -> Result<()> {
        let now = now_ms();
        self.update_shard(kind, name, |shard| {
            let mut changed = prune_receipts(kind, name, &mut shard.receipts, now);
            let fingerprint = shard
                .receipts
                .get(key)
                .and_then(receipt_fingerprint)
                .cloned();
            let next = ConnectorIngressReceiptState::Submitted {
                run_id: run_id.to_string(),
                recorded_at_ms: now,
                fingerprint,
            };
            if shard.receipts.get(key) != Some(&next) {
                shard.receipts.insert(key.to_string(), next);
                changed = true;
            }
            Ok(((), changed))
        })
        .await
    }

    /// Clears one connector ingress reservation or receipt.
    pub(crate) async fn forget_connector_ingress(
        &self,
        kind: ConnectorKind,
        name: &str,
        key: &str,
    ) -> Result<()> {
        self.update_shard(kind, name, |shard| {
            Ok(((), shard.receipts.remove(key).is_some()))
        })
        .await
    }

    /// Migrates legacy cursor and receipt state out of the daemon index into shard files.
    pub(crate) async fn migrate_legacy_index_state(
        &self,
        index: &mut SessionIndex,
    ) -> Result<bool> {
        let legacy_cursors = std::mem::take(&mut index.connector_cursors);
        let legacy_receipts = std::mem::take(&mut index.connector_ingress_receipts);
        let legacy_state_present = !legacy_cursors.is_empty() || !legacy_receipts.is_empty();
        if !legacy_state_present {
            return Ok(false);
        }

        let mut shards = self.shards.lock().await;
        let mut changed = false;

        for (raw_key, cursor) in legacy_cursors {
            let Some(key) = parse_legacy_cursor_key(&raw_key, &cursor) else {
                warn!(cursor_key = %raw_key, "ignoring unrecognized legacy connector cursor key");
                continue;
            };
            let mut shard = match shards.get(&key) {
                Some(existing) => existing.clone(),
                None => self.store.load_shard(&key)?,
            };
            let next_cursor = merge_cursor_state(shard.cursor.as_ref(), &cursor);
            if shard.cursor.as_ref() != Some(&next_cursor) {
                shard.cursor = Some(next_cursor);
                self.store.save_shard(&key, &shard)?;
                shards.insert(key, shard);
                changed = true;
            }
        }

        let mut grouped_receipts: BTreeMap<
            ConnectorShardKey,
            BTreeMap<String, ConnectorIngressReceiptState>,
        > = BTreeMap::new();
        for (raw_key, receipt) in legacy_receipts {
            let Some((key, receipt_key)) = parse_legacy_receipt_key(&raw_key) else {
                warn!(receipt_key = %raw_key, "ignoring unrecognized legacy connector ingress receipt key");
                continue;
            };
            grouped_receipts
                .entry(key)
                .or_default()
                .insert(receipt_key, receipt);
        }

        for (key, receipts) in grouped_receipts {
            let mut shard = match shards.get(&key) {
                Some(existing) => existing.clone(),
                None => self.store.load_shard(&key)?,
            };
            let mut shard_changed = false;
            for (receipt_key, receipt) in receipts {
                let replace = shard
                    .receipts
                    .get(&receipt_key)
                    .map(|existing| should_replace_receipt(existing, &receipt))
                    .unwrap_or(true);
                if replace {
                    shard.receipts.insert(receipt_key, receipt);
                    shard_changed = true;
                }
            }
            shard_changed |= prune_receipts(key.kind, &key.name, &mut shard.receipts, now_ms());
            if shard_changed {
                self.store.save_shard(&key, &shard)?;
                shards.insert(key, shard);
                changed = true;
            }
        }

        Ok(changed || legacy_state_present)
    }
}

fn merge_cursor_state(
    existing: Option<&ConnectorCursorState>,
    incoming: &ConnectorCursorState,
) -> ConnectorCursorState {
    match (existing, incoming) {
        (
            Some(ConnectorCursorState::TelegramPolling {
                next_update_id: existing,
            }),
            ConnectorCursorState::TelegramPolling {
                next_update_id: incoming,
            },
        ) => ConnectorCursorState::TelegramPolling {
            next_update_id: (*existing).max(*incoming),
        },
        (None, incoming) => incoming.clone(),
    }
}

fn should_replace_receipt(
    existing: &ConnectorIngressReceiptState,
    incoming: &ConnectorIngressReceiptState,
) -> bool {
    let existing_recorded_at = receipt_recorded_at_ms(existing);
    let incoming_recorded_at = receipt_recorded_at_ms(incoming);
    if incoming_recorded_at != existing_recorded_at {
        return incoming_recorded_at > existing_recorded_at;
    }
    matches!(
        (existing, incoming),
        (
            ConnectorIngressReceiptState::Pending { .. },
            ConnectorIngressReceiptState::Submitted { .. }
        )
    )
}

fn ensure_compatible_fingerprint(existing: Option<&str>, incoming: Option<&str>) -> Result<()> {
    if let (Some(existing), Some(incoming)) = (existing, incoming)
        && existing != incoming
    {
        anyhow::bail!(
            "connector ingress idempotency key was already submitted with a different payload"
        );
    }
    Ok(())
}

fn receipt_fingerprint(receipt: &ConnectorIngressReceiptState) -> Option<&String> {
    match receipt {
        ConnectorIngressReceiptState::Pending { fingerprint, .. }
        | ConnectorIngressReceiptState::Submitted { fingerprint, .. } => fingerprint.as_ref(),
    }
}

fn prune_receipts(
    kind: ConnectorKind,
    name: &str,
    receipts: &mut BTreeMap<String, ConnectorIngressReceiptState>,
    now_ms: u64,
) -> bool {
    let len_before = receipts.len();
    receipts.retain(|_, receipt| !receipt_is_stale(receipt, now_ms));
    if receipts.len() <= MAX_CONNECTOR_INGRESS_RECEIPTS_PER_CONNECTOR {
        return receipts.len() != len_before;
    }

    let mut keys_by_age = receipts
        .iter()
        .map(|(key, receipt)| (key.clone(), receipt_recorded_at_ms(receipt)))
        .collect::<Vec<_>>();
    keys_by_age.sort_by_key(|(_, recorded_at_ms)| *recorded_at_ms);
    let overflow = keys_by_age
        .len()
        .saturating_sub(MAX_CONNECTOR_INGRESS_RECEIPTS_PER_CONNECTOR);
    warn!(
        kind = %kind,
        connector = %name,
        overflow,
        retained = MAX_CONNECTOR_INGRESS_RECEIPTS_PER_CONNECTOR,
        "connector ingress receipts exceeded the configured per-connector retention window; pruning oldest receipts"
    );
    for (key, _) in keys_by_age.into_iter().take(overflow) {
        receipts.remove(&key);
    }
    receipts.len() != len_before
}

fn receipt_is_stale(receipt: &ConnectorIngressReceiptState, now_ms: u64) -> bool {
    match receipt {
        ConnectorIngressReceiptState::Pending { recorded_at_ms, .. } => {
            now_ms.saturating_sub(*recorded_at_ms) > MAX_CONNECTOR_INGRESS_PENDING_AGE_MS
        }
        ConnectorIngressReceiptState::Submitted { recorded_at_ms, .. } => {
            now_ms.saturating_sub(*recorded_at_ms) > MAX_CONNECTOR_INGRESS_RECEIPT_AGE_MS
        }
    }
}

fn receipt_recorded_at_ms(receipt: &ConnectorIngressReceiptState) -> u64 {
    match receipt {
        ConnectorIngressReceiptState::Pending { recorded_at_ms, .. }
        | ConnectorIngressReceiptState::Submitted { recorded_at_ms, .. } => *recorded_at_ms,
    }
}

fn parse_legacy_cursor_key(key: &str, cursor: &ConnectorCursorState) -> Option<ConnectorShardKey> {
    match cursor {
        ConnectorCursorState::TelegramPolling { .. } => parse_legacy_polling_cursor_key(key),
    }
}

fn parse_legacy_polling_cursor_key(key: &str) -> Option<ConnectorShardKey> {
    key.strip_prefix("telegram_polling:")
        .map(|name| ConnectorShardKey::new(ConnectorKind::Telegram, name))
}

fn parse_legacy_receipt_key(raw_key: &str) -> Option<(ConnectorShardKey, String)> {
    let mut parts = raw_key.splitn(3, ':');
    let kind = ConnectorKind::parse(parts.next()?).ok()?;
    let connector = parts.next()?;
    let receipt_key = parts.next()?;
    Some((
        ConnectorShardKey::new(kind, connector),
        receipt_key.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use kheish_session::{resolve_storage_path_for_read, safe_storage_name};
    use tempfile::tempdir;

    use super::ConnectorIngressService;
    use crate::connectors::ConnectorKind;
    use crate::state::{
        ConnectorCursorState, ConnectorIngressLookup, ConnectorIngressReceiptState,
        ConnectorIngressReservation, SessionIndex,
    };

    #[tokio::test]
    async fn connector_ingress_service_round_trips_one_shard() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let service = ConnectorIngressService::load(temp.path());

        assert_eq!(
            service
                .begin_connector_ingress(ConnectorKind::Http, "ingress", "req-1")
                .await?,
            ConnectorIngressReservation::Reserved
        );
        service
            .remember_connector_ingress_run(ConnectorKind::Http, "ingress", "req-1", "run-1")
            .await?;
        assert_eq!(
            service
                .begin_connector_ingress(ConnectorKind::Http, "ingress", "req-1")
                .await?,
            ConnectorIngressReservation::Existing {
                run_id: "run-1".to_string()
            }
        );
        service
            .remember_connector_next_update_id(ConnectorKind::Telegram, "ops-bot", 42)
            .await?;
        assert_eq!(
            service
                .connector_next_update_id(ConnectorKind::Telegram, "ops-bot")
                .await,
            Some(42)
        );
        Ok(())
    }

    #[tokio::test]
    async fn connector_ingress_service_lookup_does_not_create_receipt() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let service = ConnectorIngressService::load(temp.path());

        assert_eq!(
            service
                .lookup_connector_ingress(ConnectorKind::External, "discord", "evt-1")
                .await?,
            ConnectorIngressLookup::Absent
        );
        assert!(
            !resolve_storage_path_for_read(
                &temp.path().join("connector-ingress").join("external"),
                "discord",
                "json"
            )
            .exists(),
            "lookup of an absent event must not create a shard file"
        );

        assert_eq!(
            service
                .begin_connector_ingress(ConnectorKind::External, "discord", "evt-1")
                .await?,
            ConnectorIngressReservation::Reserved
        );
        assert_eq!(
            service
                .lookup_connector_ingress(ConnectorKind::External, "discord", "evt-1")
                .await?,
            ConnectorIngressLookup::Pending
        );
        service
            .remember_connector_ingress_run(ConnectorKind::External, "discord", "evt-1", "run-1")
            .await?;
        assert_eq!(
            service
                .lookup_connector_ingress(ConnectorKind::External, "discord", "evt-1")
                .await?,
            ConnectorIngressLookup::Existing {
                run_id: "run-1".to_string()
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn connector_ingress_service_rejects_fingerprint_mismatch() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let service = ConnectorIngressService::load(temp.path());

        assert_eq!(
            service
                .begin_connector_ingress_with_fingerprint(
                    ConnectorKind::Http,
                    "ingress",
                    "req-1",
                    "fingerprint-a",
                )
                .await?,
            ConnectorIngressReservation::Reserved
        );
        let error = service
            .begin_connector_ingress_with_fingerprint(
                ConnectorKind::Http,
                "ingress",
                "req-1",
                "fingerprint-b",
            )
            .await
            .expect_err("same key with a different fingerprint should fail");
        assert!(
            error.to_string().contains("different payload"),
            "unexpected error: {error:#}"
        );

        service
            .remember_connector_ingress_run(ConnectorKind::Http, "ingress", "req-1", "run-1")
            .await?;
        let same = service
            .begin_connector_ingress_with_fingerprint(
                ConnectorKind::Http,
                "ingress",
                "req-1",
                "fingerprint-a",
            )
            .await?;
        assert_eq!(
            same,
            ConnectorIngressReservation::Existing {
                run_id: "run-1".to_string()
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn connector_ingress_service_migrates_legacy_index_state() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let service = ConnectorIngressService::load(temp.path());
        let mut index = SessionIndex::default();
        index.connector_cursors.insert(
            "telegram_polling:ops-bot".to_string(),
            ConnectorCursorState::TelegramPolling { next_update_id: 17 },
        );
        index.connector_ingress_receipts.insert(
            "http:ingress:req-1".to_string(),
            ConnectorIngressReceiptState::Submitted {
                run_id: "run-1".to_string(),
                recorded_at_ms: crate::now_ms(),
                fingerprint: None,
            },
        );

        assert!(service.migrate_legacy_index_state(&mut index).await?);
        assert!(index.connector_cursors.is_empty());
        assert!(index.connector_ingress_receipts.is_empty());
        assert_eq!(
            service
                .connector_next_update_id(ConnectorKind::Telegram, "ops-bot")
                .await,
            Some(17)
        );
        assert_eq!(
            service
                .begin_connector_ingress(ConnectorKind::Http, "ingress", "req-1")
                .await?,
            ConnectorIngressReservation::Existing {
                run_id: "run-1".to_string()
            }
        );
        assert!(
            resolve_storage_path_for_read(
                &temp.path().join("connector-ingress").join("http"),
                "ingress",
                "json"
            )
            .exists()
        );
        Ok(())
    }

    #[test]
    fn safe_storage_name_handles_hostile_connector_names() {
        let unsafe_name = "../../../etc/passwd";
        assert_ne!(safe_storage_name(unsafe_name), unsafe_name);
    }
}
