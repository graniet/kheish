use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use kheish_session::write_json_pretty_atomically;

use super::ScheduleRecord;
use crate::state_files::read_json_or_quarantine;

/// Filesystem-backed persistence for daemon schedules.
#[derive(Clone, Debug)]
pub(crate) struct FileScheduleStore {
    root: PathBuf,
}

impl FileScheduleStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn schedules_root(&self) -> PathBuf {
        self.root.join("schedules")
    }

    fn schedule_path(&self, schedule_id: &str) -> PathBuf {
        self.schedules_root().join(format!("{schedule_id}.json"))
    }

    pub(crate) fn load_schedules(&self) -> Result<BTreeMap<String, ScheduleRecord>> {
        let root = self.schedules_root();
        if !root.exists() {
            return Ok(BTreeMap::new());
        }
        let mut records = BTreeMap::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(record) = read_json_or_quarantine::<ScheduleRecord>(&path, "schedule record")?
            else {
                continue;
            };
            records.insert(record.view.schedule_id.clone(), record);
        }
        Ok(records)
    }

    pub(crate) fn save_schedule(&self, record: &ScheduleRecord) -> Result<()> {
        let path = self.schedule_path(&record.view.schedule_id);
        write_json_pretty_atomically(&path, record)
    }

    pub(crate) fn next_seed(&self) -> u64 {
        self.schedule_file_paths()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| self.schedule_id_from_path(&path))
            .filter_map(|id| {
                id.strip_prefix("schedule-")
                    .and_then(|value| value.parse().ok())
            })
            .max()
            .unwrap_or(0u64)
            .saturating_add(1)
    }
}

impl FileScheduleStore {
    fn schedule_file_paths(&self) -> Result<Vec<PathBuf>> {
        let root = self.schedules_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    fn schedule_id_from_path(&self, path: &std::path::Path) -> Option<String> {
        let file_name = path.file_name()?.to_str()?;
        let base_name = file_name.split(".corrupt-").next().unwrap_or(file_name);
        let stem = base_name.strip_suffix(".json")?;
        Some(stem.to_string())
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    #[test]
    fn schedule_store_next_seed_preserves_quarantined_highest_schedule_id() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FileScheduleStore::new(root.path());
        fs::create_dir_all(store.schedules_root())?;
        fs::write(store.schedule_path("schedule-1"), "{}")?;
        fs::write(store.schedule_path("schedule-2"), "{}")?;

        let schedule_two = store.schedule_path("schedule-2");
        let quarantined = schedule_two.with_file_name(format!(
            "{}.corrupt-test",
            schedule_two
                .file_name()
                .expect("schedule-2 file")
                .to_string_lossy()
        ));
        fs::rename(&schedule_two, &quarantined)?;

        assert_eq!(store.next_seed(), 3);
        Ok(())
    }
}
