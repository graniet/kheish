use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable metadata key used to carry daemon-recovered run memory into a prompt.
pub const RECOVERED_MEMORY_METADATA_KEY: &str = "recovered_memory";

/// One compact recovered-memory entry derived from a prior daemon run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredMemoryEntry {
    /// The originating run identifier.
    pub run_id: String,
    /// The time when the memory record was captured.
    pub recorded_at_ms: u64,
    /// The terminal run status captured for operator-facing context.
    pub status: String,
    /// Optional preview of the original request retained for retrieval and debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_preview: Option<String>,
    /// Optional preview of the terminal outcome retained for retrieval and debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_preview: Option<String>,
    /// Optional daemon-owned artifact identifiers touched by the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_ids: Vec<String>,
    /// Optional compact failure markers derived from terminal errors or interrupted states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_markers: Vec<String>,
    /// The compact memory summary injected back into the prompt.
    pub summary: String,
}

/// A bounded set of recovered run memories prepared for prompt injection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredMemoryBundle {
    /// The ordered recovered-memory entries, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<RecoveredMemoryEntry>,
    /// Whether older or larger memories were omitted while packing the bundle.
    #[serde(default)]
    pub truncated: bool,
}

impl RecoveredMemoryBundle {
    /// Returns true when the bundle carries no prompt-visible memory.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Extracts recovered memory from normalized input metadata.
pub fn recovered_memory_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<RecoveredMemoryBundle>> {
    metadata
        .get(RECOVERED_MEMORY_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value::<RecoveredMemoryBundle>)
        .transpose()
        .map(|bundle| bundle.filter(|bundle| !bundle.is_empty()))
}

/// Returns metadata with recovered memory merged in under the stable key.
pub fn metadata_with_recovered_memory(
    metadata: Value,
    bundle: Option<&RecoveredMemoryBundle>,
) -> serde_json::Result<Value> {
    let Some(bundle) = bundle.filter(|bundle| !bundle.is_empty()) else {
        return Ok(metadata);
    };

    let mut object = match metadata {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("user_metadata".to_string(), other);
            map
        }
    };
    object.insert(
        RECOVERED_MEMORY_METADATA_KEY.to_string(),
        serde_json::to_value(bundle)?,
    );
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use serde_json::json;

    use super::{
        RECOVERED_MEMORY_METADATA_KEY, RecoveredMemoryBundle, RecoveredMemoryEntry,
        metadata_with_recovered_memory, recovered_memory_from_metadata,
    };

    fn sample_bundle() -> RecoveredMemoryBundle {
        RecoveredMemoryBundle {
            entries: vec![RecoveredMemoryEntry {
                run_id: "run-1".to_string(),
                recorded_at_ms: 42,
                status: "completed".to_string(),
                request_preview: Some("inspect".to_string()),
                outcome_preview: Some("ok".to_string()),
                artifact_ids: vec!["asset-1".to_string()],
                failure_markers: Vec::new(),
                summary: "Request: inspect\nResult: ok".to_string(),
            }],
            truncated: false,
        }
    }

    #[test]
    fn recovered_memory_round_trips_through_metadata() {
        let metadata = metadata_with_recovered_memory(Value::Null, Some(&sample_bundle()))
            .expect("metadata should serialize");
        let bundle = recovered_memory_from_metadata(&metadata)
            .expect("metadata should deserialize")
            .expect("bundle should exist");
        assert_eq!(bundle, sample_bundle());
    }

    #[test]
    fn recovered_memory_wraps_non_object_metadata() {
        let metadata = metadata_with_recovered_memory(json!("legacy"), Some(&sample_bundle()))
            .expect("metadata should serialize");
        assert_eq!(metadata["user_metadata"], json!("legacy"));
        assert!(metadata.get(RECOVERED_MEMORY_METADATA_KEY).is_some());
    }

    #[test]
    fn recovered_memory_ignores_empty_bundles() {
        let empty = RecoveredMemoryBundle::default();
        let metadata = metadata_with_recovered_memory(json!({"existing": true}), Some(&empty))
            .expect("metadata should serialize");
        assert_eq!(metadata, json!({"existing": true}));
        assert_eq!(
            recovered_memory_from_metadata(&json!({
                RECOVERED_MEMORY_METADATA_KEY: {
                    "entries": [],
                    "truncated": false
                }
            }))
            .expect("metadata should deserialize"),
            None
        );
    }
}
