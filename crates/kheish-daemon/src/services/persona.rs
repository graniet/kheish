use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::personas::{FilePersonaStore, PersonaIndex, PersonaIndexEntry, PersonaRecord};

/// Owns durable persona records and the compact persona index.
pub(crate) struct PersonaService {
    store: FilePersonaStore,
    index: Mutex<PersonaIndex>,
    next_persona_id: AtomicU64,
}

impl PersonaService {
    /// Creates a new persona service backed by the persisted persona store.
    pub(crate) fn new(store: FilePersonaStore, index: PersonaIndex) -> Self {
        Self {
            next_persona_id: AtomicU64::new(index.next_persona_id.max(1)),
            store,
            index: Mutex::new(index),
        }
    }

    /// Returns one fresh daemon-managed persona identifier.
    pub(crate) fn next_persona_id(&self) -> String {
        loop {
            let candidate = format!(
                "persona-{}",
                self.next_persona_id.fetch_add(1, Ordering::Relaxed)
            );
            if self.store.load_persona(&candidate).ok().flatten().is_none() {
                return candidate;
            }
        }
    }

    /// Lists the compact persona summaries tracked by the daemon.
    pub(crate) async fn list_personas(&self) -> Vec<PersonaIndexEntry> {
        let mut personas = self
            .index
            .lock()
            .await
            .personas
            .values()
            .cloned()
            .collect::<Vec<_>>();
        personas.sort_by(|left, right| left.persona_id.cmp(&right.persona_id));
        personas
    }

    /// Loads one full persona record by identifier.
    pub(crate) async fn get_persona(&self, persona_id: &str) -> Result<PersonaRecord> {
        match self.store.load_persona(persona_id)? {
            Some(record) => Ok(record),
            None => {
                self.remove_stale_index_entry(persona_id).await?;
                Err(anyhow!("unknown persona {persona_id}"))
            }
        }
    }

    /// Persists one new persona record and index entry.
    pub(crate) async fn create_persona(&self, record: PersonaRecord) -> Result<PersonaRecord> {
        let mut index = self.index.lock().await;
        if index.personas.contains_key(&record.persona_id) {
            bail!("persona {} already exists", record.persona_id);
        }
        if self.store.load_persona(&record.persona_id)?.is_some() {
            bail!("persona {} already exists", record.persona_id);
        }
        self.store.save_persona(&record)?;
        let previous_next_persona_id = index.next_persona_id;
        advance_persona_seed(&mut index, &record.persona_id);
        index
            .personas
            .insert(record.persona_id.clone(), PersonaIndexEntry::from(&record));
        if let Err(error) = self.store.save_index(&index) {
            index.personas.remove(&record.persona_id);
            index.next_persona_id = previous_next_persona_id;
            return match self.store.delete_persona(&record.persona_id) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist persona index after writing {}; rollback also failed: {rollback_error}",
                    record.persona_id
                )),
            };
        }
        Ok(record)
    }

    /// Updates one persisted persona record in place.
    pub(crate) async fn update_persona(
        &self,
        persona_id: &str,
        update: impl FnOnce(&mut PersonaRecord) -> Result<bool>,
    ) -> Result<PersonaRecord> {
        let mut record = self.get_persona(persona_id).await?;
        let previous_record = record.clone();
        if !update(&mut record)? {
            return Ok(previous_record);
        }
        self.store.save_persona(&record)?;
        let mut index = self.index.lock().await;
        let previous_entry = index.personas.get(persona_id).cloned();
        let previous_next_persona_id = index.next_persona_id;
        index
            .personas
            .insert(persona_id.to_string(), PersonaIndexEntry::from(&record));
        advance_persona_seed(&mut index, &record.persona_id);
        if let Err(error) = self.store.save_index(&index) {
            match previous_entry {
                Some(entry) => {
                    index.personas.insert(persona_id.to_string(), entry);
                }
                None => {
                    index.personas.remove(persona_id);
                }
            }
            index.next_persona_id = previous_next_persona_id;
            return match self.store.save_persona(&previous_record) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "failed to persist persona index after updating {persona_id}; rollback also failed: {rollback_error}"
                )),
            };
        }
        Ok(record)
    }

    async fn remove_stale_index_entry(&self, persona_id: &str) -> Result<()> {
        let mut index = self.index.lock().await;
        if index.personas.remove(persona_id).is_some() {
            self.store.save_index(&index)?;
        }
        Ok(())
    }
}

fn advance_persona_seed(index: &mut PersonaIndex, persona_id: &str) {
    let Some(numeric) = persona_id
        .strip_prefix("persona-")
        .and_then(|suffix| suffix.parse::<u64>().ok())
    else {
        return;
    };
    index.next_persona_id = index.next_persona_id.max(numeric.saturating_add(1));
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn test_record(persona_id: &str, display_name: &str) -> PersonaRecord {
        PersonaRecord {
            persona_id: persona_id.to_string(),
            display_name: display_name.to_string(),
            soul: format!("Reply as {display_name}."),
            capability_scope: Default::default(),
            default_skills: Vec::new(),
            version: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            metadata: json!({"fixture": true}),
        }
    }

    #[tokio::test]
    async fn create_persona_rolls_back_record_when_index_persist_fails() -> Result<()> {
        let temp = tempdir()?;
        let store = FilePersonaStore::new(temp.path());
        fs::create_dir_all(temp.path().join("persona-index.json"))?;
        let service = PersonaService::new(store.clone(), PersonaIndex::default());

        let error = service
            .create_persona(test_record("persona-create-rollback", "Create Rollback"))
            .await
            .expect_err("create should fail when persona-index.json is blocked by a directory");
        assert!(
            error.to_string().contains("persona-index.json"),
            "unexpected create failure: {error}"
        );
        assert!(
            store.load_persona("persona-create-rollback")?.is_none(),
            "failed create should not leave a durable persona record behind"
        );
        Ok(())
    }

    #[tokio::test]
    async fn update_persona_restores_previous_record_when_index_persist_fails() -> Result<()> {
        let temp = tempdir()?;
        let store = FilePersonaStore::new(temp.path());
        let service = PersonaService::new(store.clone(), PersonaIndex::default());
        service
            .create_persona(test_record("persona-update-rollback", "Before"))
            .await?;

        fs::remove_file(temp.path().join("persona-index.json"))?;
        fs::create_dir_all(temp.path().join("persona-index.json"))?;

        let error = service
            .update_persona("persona-update-rollback", |record| {
                record.display_name = "After".to_string();
                Ok(true)
            })
            .await
            .expect_err("update should fail when persona-index.json is blocked by a directory");
        assert!(
            error.to_string().contains("persona-index.json"),
            "unexpected update failure: {error}"
        );
        let restored = store
            .load_persona("persona-update-rollback")?
            .expect("previous persona record should remain readable after rollback");
        assert_eq!(restored.display_name, "Before");
        Ok(())
    }
}
