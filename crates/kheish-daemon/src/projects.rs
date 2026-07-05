//! Durable project records and project-task records owned by the daemon state root.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kheish_session::{
    decode_safe_storage_name, prepare_storage_path_for_write, resolve_storage_path_for_read,
    write_json_pretty_atomically,
};
use kheish_types::TaskStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::channels::ChannelMemberKind;
use crate::state_files::read_json_or_quarantine;

/// The durable lifecycle state for one project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// The project is active and accepts new work.
    Active,
    /// The project is temporarily paused.
    Paused,
    /// The project reached its intended outcome.
    Completed,
    /// The project is retained only for history.
    Archived,
}

impl Default for ProjectStatus {
    fn default() -> Self {
        Self::Active
    }
}

/// One durable project member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMemberView {
    /// The stable member identifier inside the project namespace.
    pub member_id: String,
    /// The durable member kind.
    pub member_kind: ChannelMemberKind,
    /// The human-readable member display name.
    pub display_name: String,
    /// The bound daemon session identifier when this member is session-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The bound daemon actor identifier when this member is human-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// The optional role label used for work assignment and routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Optional expertise tags used by operators and automation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expertise_tags: Vec<String>,
    /// The timestamp when the member joined the project.
    pub joined_at_ms: u64,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One durable channel link attached to a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectChannelLinkView {
    /// The daemon-owned channel identifier.
    pub channel_id: String,
    /// The optional semantic role of the channel inside the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Whether new tasks should default to this channel when no explicit discussion channel is set.
    #[serde(default)]
    pub default_for_new_tasks: bool,
    /// Whether operators want project members mirrored into this channel.
    #[serde(default)]
    pub mirror_members: bool,
    /// The timestamp when the channel link was created.
    pub linked_at_ms: u64,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One compact project summary returned by list APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummaryView {
    /// The stable daemon-owned project identifier.
    pub project_id: String,
    /// The user-visible project name.
    pub display_name: String,
    /// The optional short project description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The current project lifecycle state.
    #[serde(default)]
    pub status: ProjectStatus,
    /// The current member count.
    #[serde(default)]
    pub member_count: u64,
    /// The current linked-channel count.
    #[serde(default)]
    pub channel_count: u64,
    /// The current task count.
    #[serde(default)]
    pub task_count: u64,
    /// The current non-terminal task count.
    #[serde(default)]
    pub active_task_count: u64,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The last update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// One full project record returned by detail APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    /// The compact externally visible project summary.
    #[serde(flatten)]
    pub summary: ProjectSummaryView,
    /// The durable project members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ProjectMemberView>,
    /// The durable linked project channels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channel_links: Vec<ProjectChannelLinkView>,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// One optional discussion anchor attached to a project task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskDiscussionRef {
    /// The linked project channel that holds the canonical discussion.
    pub channel_id: String,
    /// The public thread root identifier used for the task discussion.
    pub thread_root_message_id: String,
}

/// One durable task owned by a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTaskView {
    /// The stable daemon-owned project-task identifier.
    pub project_task_id: String,
    /// The owning project identifier.
    pub project_id: String,
    /// The short user-visible task title.
    pub title: String,
    /// The longer task description.
    pub description: String,
    /// The current task lifecycle state.
    pub status: TaskStatus,
    /// The assigned project member identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_member_id: Option<String>,
    /// The assigned member session identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_session_id: Option<String>,
    /// The latest run identifier directly associated with this task when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<String>,
    /// The optional public discussion anchor for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion: Option<ProjectTaskDiscussionRef>,
    /// The task identifiers that currently block this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// The parent task identifier when this task is a subtask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// Optional recorded task output or conclusion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The creation timestamp in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The last update timestamp in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Optional caller-supplied metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: Value,
}

/// Filesystem-backed project storage rooted under one daemon state directory.
#[derive(Clone, Debug)]
pub(crate) struct FileProjectStore {
    root: PathBuf,
}

impl FileProjectStore {
    /// Creates a new project store rooted under one daemon state directory.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn projects_root(&self) -> PathBuf {
        self.root.join("projects")
    }

    fn tasks_root(&self) -> PathBuf {
        self.root.join("project-tasks")
    }

    fn project_path(&self, project_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.projects_root(), project_id, "json")
    }

    fn task_path(&self, task_id: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.tasks_root(), task_id, "json")
    }

    /// Loads every persisted project record, quarantining corrupted files.
    pub(crate) fn load_projects(&self) -> Result<BTreeMap<String, ProjectView>> {
        load_json_records::<ProjectView>(&self.projects_root(), "project record").map(|records| {
            let mut map = BTreeMap::new();
            for record in records {
                map.insert(record.summary.project_id.clone(), record);
            }
            map
        })
    }

    /// Loads every persisted project-task record, quarantining corrupted files.
    pub(crate) fn load_tasks(&self) -> Result<BTreeMap<String, ProjectTaskView>> {
        load_json_records::<ProjectTaskView>(&self.tasks_root(), "project task record").map(
            |records| {
                let mut map = BTreeMap::new();
                for record in records {
                    map.insert(record.project_task_id.clone(), record);
                }
                map
            },
        )
    }

    /// Persists one project record atomically.
    pub(crate) fn save_project(&self, project: &ProjectView) -> Result<()> {
        let path = prepare_storage_path_for_write(
            &self.projects_root(),
            &project.summary.project_id,
            "json",
        )?;
        write_json_pretty_atomically(&path, project)
    }

    /// Persists one project-task record atomically.
    pub(crate) fn save_task(&self, task: &ProjectTaskView) -> Result<()> {
        let path =
            prepare_storage_path_for_write(&self.tasks_root(), &task.project_task_id, "json")?;
        write_json_pretty_atomically(&path, task)
    }

    /// Deletes one persisted project record when it exists.
    pub(crate) fn delete_project(&self, project_id: &str) -> Result<()> {
        delete_if_exists(self.project_path(project_id), "project record")
    }

    /// Deletes one persisted project-task record when it exists.
    pub(crate) fn delete_task(&self, task_id: &str) -> Result<()> {
        delete_if_exists(self.task_path(task_id), "project task record")
    }

    /// Returns the next numeric project identifier seed.
    pub(crate) fn next_project_seed(&self) -> u64 {
        next_seed_from_root(&self.projects_root(), "project-")
    }

    /// Returns the next numeric project-task identifier seed.
    pub(crate) fn next_task_seed(&self) -> u64 {
        next_seed_from_root(&self.tasks_root(), "project-task-")
    }
}

fn delete_if_exists(path: PathBuf, label: &str) -> Result<()> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete {label} {}", path.display()))
        }
    }
}

fn load_json_records<T>(root: &Path, label: &'static str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for scan_root in storage_scan_roots(root) {
        if !scan_root.exists() {
            continue;
        }
        for entry in fs::read_dir(scan_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = read_json_or_quarantine::<T>(&path, label)? {
                records.push(record);
            }
        }
    }
    Ok(records)
}

fn next_seed_from_root(root: &Path, prefix: &str) -> u64 {
    storage_scan_roots(root)
        .into_iter()
        .filter_map(|scan_root| fs::read_dir(scan_root).ok())
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| storage_identifier_from_path(&entry.path()))
        .filter_map(|identifier| {
            identifier
                .strip_prefix(prefix)
                .and_then(|suffix| suffix.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn storage_scan_roots(root: &Path) -> [PathBuf; 2] {
    [root.to_path_buf(), root.join("__safe")]
}

fn storage_identifier_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let stem = stem.split(".corrupt-").next().unwrap_or(stem);
    let identifier = stem.strip_suffix(".json").unwrap_or(stem);
    decode_safe_storage_name(identifier).or_else(|| Some(identifier.to_string()))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    use super::*;

    fn fixture_project(project_id: &str) -> ProjectView {
        ProjectView {
            summary: ProjectSummaryView {
                project_id: project_id.to_string(),
                display_name: "alpha".to_string(),
                description: None,
                status: ProjectStatus::Active,
                member_count: 1,
                channel_count: 0,
                task_count: 0,
                active_task_count: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            members: Vec::new(),
            channel_links: Vec::new(),
            metadata: Value::Null,
        }
    }

    fn fixture_task(task_id: &str, project_id: &str) -> ProjectTaskView {
        ProjectTaskView {
            project_task_id: task_id.to_string(),
            project_id: project_id.to_string(),
            title: "do work".to_string(),
            description: "desc".to_string(),
            status: TaskStatus::Pending,
            assignee_member_id: None,
            primary_session_id: None,
            parent_task_id: None,
            latest_run_id: None,
            discussion: None,
            blocked_by: Vec::new(),
            output: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            metadata: json!({"fixture": true}),
        }
    }

    #[test]
    fn next_seeds_follow_existing_records() -> Result<()> {
        let temp = tempdir()?;
        let store = FileProjectStore::new(temp.path());
        store.save_project(&fixture_project("project-7"))?;
        store.save_task(&fixture_task("project-task-9", "project-7"))?;

        assert_eq!(store.next_project_seed(), 8);
        assert_eq!(store.next_task_seed(), 10);
        Ok(())
    }

    #[test]
    fn project_store_roundtrips_and_deletes_records() -> Result<()> {
        let temp = tempdir()?;
        let store = FileProjectStore::new(temp.path());
        let project = fixture_project("project-roundtrip");
        let task = fixture_task("project-task-roundtrip", "project-roundtrip");

        store.save_project(&project)?;
        store.save_task(&task)?;
        assert_eq!(
            store.load_projects()?.get("project-roundtrip"),
            Some(&project)
        );
        assert_eq!(
            store.load_tasks()?.get("project-task-roundtrip"),
            Some(&task)
        );

        store.delete_task("project-task-roundtrip")?;
        store.delete_project("project-roundtrip")?;
        assert!(store.load_tasks()?.is_empty());
        assert!(store.load_projects()?.is_empty());
        Ok(())
    }

    #[test]
    fn corrupt_project_files_are_quarantined_on_load() -> Result<()> {
        let temp = tempdir()?;
        let store = FileProjectStore::new(temp.path());
        fs::create_dir_all(temp.path().join("projects"))?;
        fs::write(
            temp.path().join("projects/project-corrupt.json"),
            "{not valid json",
        )?;

        let projects = store.load_projects()?;
        assert!(projects.is_empty());
        assert!(fs::read_dir(temp.path().join("projects"))?.any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.contains("project-corrupt.json.corrupt-"))
        }));
        Ok(())
    }
}
