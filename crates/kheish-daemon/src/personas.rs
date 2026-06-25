//! Durable persona records and indexes owned by the daemon state root.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use kheish_session::{
    prepare_storage_path_for_write, resolve_storage_path_for_read, write_json_pretty_atomically,
};
use kheish_types::{CapabilityScope, PersonaSkillAssignment};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state_files::read_json_or_quarantine;

/// One persisted persona record stored under the daemon state root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersonaRecord {
    /// The stable daemon-owned persona identifier.
    pub(crate) persona_id: String,
    /// The user-visible persona name.
    pub(crate) display_name: String,
    /// The exact persona instructions bound into sessions.
    pub(crate) soul: String,
    /// The persona-scoped capability baseline applied to bound sessions.
    #[serde(default, skip_serializing_if = "CapabilityScope::is_empty")]
    pub(crate) capability_scope: CapabilityScope,
    /// The inline skills activated by default for sessions bound to this persona.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) default_skills: Vec<PersonaSkillAssignment>,
    /// The monotonically increasing persona version.
    pub(crate) version: u64,
    /// The creation timestamp in milliseconds.
    pub(crate) created_at_ms: u64,
    /// The last update timestamp in milliseconds.
    pub(crate) updated_at_ms: u64,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub(crate) metadata: Value,
}

/// One compact persona index entry used for listing and update validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersonaIndexEntry {
    /// The stable daemon-owned persona identifier.
    pub(crate) persona_id: String,
    /// The user-visible persona name.
    pub(crate) display_name: String,
    /// The latest persona version.
    pub(crate) version: u64,
    /// The creation timestamp in milliseconds.
    pub(crate) created_at_ms: u64,
    /// The last update timestamp in milliseconds.
    pub(crate) updated_at_ms: u64,
}

impl From<&PersonaRecord> for PersonaIndexEntry {
    fn from(value: &PersonaRecord) -> Self {
        Self {
            persona_id: value.persona_id.clone(),
            display_name: value.display_name.clone(),
            version: value.version,
            created_at_ms: value.created_at_ms,
            updated_at_ms: value.updated_at_ms,
        }
    }
}

/// The durable persona index stored beside daemon state files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersonaIndex {
    /// The next numeric persona identifier seed.
    #[serde(default = "default_next_persona_id")]
    pub(crate) next_persona_id: u64,
    /// The compact persona summaries keyed by identifier.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) personas: BTreeMap<String, PersonaIndexEntry>,
}

fn default_next_persona_id() -> u64 {
    1
}

/// Filesystem-backed persona storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FilePersonaStore {
    root: PathBuf,
}

impl FilePersonaStore {
    /// Creates a new persona store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("persona-index.json")
    }

    fn persona_path(&self, persona_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.root.join("personas"), persona_id, "json")
    }

    /// Loads the persisted persona index, tolerating corrupted files by quarantining them.
    pub(crate) fn load_index(&self) -> Result<PersonaIndex> {
        Ok(read_json_or_quarantine(&self.index_path(), "persona index")?.unwrap_or_default())
    }

    /// Loads the persona index and repairs it from on-disk persona records when needed.
    pub(crate) fn load_index_repaired(&self) -> Result<PersonaIndex> {
        let mut index = self.load_index()?;
        let recovered = self
            .load_all_personas()?
            .into_iter()
            .map(|record| {
                let next_seed = persona_seed_after(&record.persona_id);
                let entry = PersonaIndexEntry::from(&record);
                (record.persona_id, entry, next_seed)
            })
            .collect::<Vec<_>>();
        let recovered_entries = recovered
            .iter()
            .map(|(persona_id, entry, _)| (persona_id.clone(), entry.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut changed = false;

        let len_before = index.personas.len();
        index
            .personas
            .retain(|persona_id, _| recovered_entries.contains_key(persona_id));
        changed |= len_before != index.personas.len();

        for (persona_id, entry, next_seed) in recovered {
            if index.personas.get(&persona_id) != Some(&entry) {
                index.personas.insert(persona_id, entry);
                changed = true;
            }
            if index.next_persona_id < next_seed {
                index.next_persona_id = next_seed;
                changed = true;
            }
        }
        if changed {
            self.save_index(&index)?;
        }
        Ok(index)
    }

    /// Persists the current persona index atomically.
    pub(crate) fn save_index(&self, index: &PersonaIndex) -> Result<()> {
        write_json_pretty_atomically(&self.index_path(), index)
    }

    /// Loads one persona record by identifier.
    pub(crate) fn load_persona(&self, persona_id: &str) -> Result<Option<PersonaRecord>> {
        read_json_or_quarantine(&self.persona_path(persona_id), "persona record")
    }

    /// Persists one persona record atomically.
    pub(crate) fn save_persona(&self, record: &PersonaRecord) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.root.join("personas"),
            &record.persona_id,
            "json",
        )?;
        write_json_pretty_atomically(&path, record)
    }

    /// Deletes one persisted persona record when it exists.
    pub(crate) fn delete_persona(&self, persona_id: &str) -> Result<()> {
        let path = prepare_storage_path_for_write(&self.root.join("personas"), persona_id, "json")?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to delete persona record {}", path.display())),
        }
    }

    fn load_all_personas(&self) -> Result<Vec<PersonaRecord>> {
        let mut records = Vec::new();
        for root in [
            self.root.join("personas"),
            self.root.join("personas").join("__safe"),
        ] {
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json")
                {
                    continue;
                }
                if let Some(record) =
                    read_json_or_quarantine::<PersonaRecord>(&path, "persona record")?
                {
                    records.push(record);
                }
            }
        }
        records.sort_by(|left, right| left.persona_id.cmp(&right.persona_id));
        records.dedup_by(|left, right| left.persona_id == right.persona_id);
        Ok(records)
    }
}

fn persona_seed_after(persona_id: &str) -> u64 {
    persona_id
        .strip_prefix("persona-")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .map(|numeric| numeric.saturating_add(1))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;

    use super::*;

    #[test]
    fn load_index_repaired_recovers_entries_from_persona_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FilePersonaStore::new(root.path());
        store.save_persona(&PersonaRecord {
            persona_id: "persona-7".to_string(),
            display_name: "Analyst".to_string(),
            soul: "Reply as Analyst.".to_string(),
            capability_scope: CapabilityScope::default(),
            default_skills: Vec::new(),
            version: 3,
            created_at_ms: 1,
            updated_at_ms: 2,
            metadata: json!({"team": "ops"}),
        })?;

        let repaired = store.load_index_repaired()?;
        assert_eq!(repaired.next_persona_id, 8);
        assert_eq!(repaired.personas.len(), 1);
        assert_eq!(
            repaired.personas.get("persona-7"),
            Some(&PersonaIndexEntry {
                persona_id: "persona-7".to_string(),
                display_name: "Analyst".to_string(),
                version: 3,
                created_at_ms: 1,
                updated_at_ms: 2,
            })
        );
        Ok(())
    }

    #[test]
    fn load_index_repaired_prunes_stale_entries_from_the_index() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FilePersonaStore::new(root.path());
        store.save_index(&PersonaIndex {
            next_persona_id: 12,
            personas: BTreeMap::from([(
                "persona-11".to_string(),
                PersonaIndexEntry {
                    persona_id: "persona-11".to_string(),
                    display_name: "Stale".to_string(),
                    version: 5,
                    created_at_ms: 1,
                    updated_at_ms: 2,
                },
            )]),
        })?;

        let repaired = store.load_index_repaired()?;
        assert_eq!(repaired.next_persona_id, 12);
        assert!(repaired.personas.is_empty());
        Ok(())
    }
}
