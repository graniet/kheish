use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable metadata key used to carry daemon-published learned context into a prompt.
pub const LEARNED_CONTEXT_METADATA_KEY: &str = "learned_context";
/// Stable identifier used by daemon-wide workspace learning records.
pub const DEFAULT_WORKSPACE_LEARNING_SCOPE_ID: &str = "default";

/// The durable scope that owns one learning artifact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningScopeKind {
    /// The learning applies only to one session.
    Session,
    /// The learning applies to sessions bound to one persona.
    Persona,
    /// The learning applies to members of one project.
    Project,
    /// The learning applies daemon-wide within the current workspace.
    Workspace,
}

/// One stable learning scope identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LearningScope {
    /// The durable scope kind.
    pub kind: LearningScopeKind,
    /// The scope identifier within the selected kind.
    pub id: String,
}

impl LearningScope {
    /// Returns true when the scope identifier is missing required content.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id.trim().is_empty()
    }

    /// Returns the stable scope key used by daemon indexes.
    #[must_use]
    pub fn scope_key(&self) -> String {
        let kind = match self.kind {
            LearningScopeKind::Session => "session",
            LearningScopeKind::Persona => "persona",
            LearningScopeKind::Project => "project",
            LearningScopeKind::Workspace => "workspace",
        };
        format!("{kind}:{}", self.id.trim())
    }
}

/// The durable semantic or procedural class carried by one learning artifact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningKind {
    /// One compact run summary captured for later review.
    RunSummary,
    /// One stable fact that should be recalled later.
    Fact,
    /// One stable user or workspace preference.
    Preference,
    /// One durable decision that affects future work.
    Decision,
    /// One reusable procedure candidate that should not be auto-injected.
    Procedure,
}

impl LearningKind {
    /// Returns whether the learning kind is eligible for prompt injection.
    #[must_use]
    pub fn is_prompt_eligible(&self) -> bool {
        !matches!(self, Self::RunSummary | Self::Procedure)
    }
}

/// The publication status of one durable learning record.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningStatus {
    /// The learning is durably stored but must stay out of normal prompt retrieval.
    Provisional,
    /// The learning remains eligible for retrieval.
    Active,
    /// The learning was replaced by a newer record.
    Superseded,
    /// The learning was explicitly revoked.
    Revoked,
}

/// The visibility class attached to one learning artifact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSensitivity {
    /// The learning is safe to reuse within its exact scope.
    Scoped,
    /// The learning should be treated as sensitive within its scope.
    Sensitive,
}

/// The publication tier assigned by daemon governance.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningPublishTier {
    /// The learning is retained for audit and shadow evaluation only.
    Provisional,
    /// The learning is eligible for normal prompt retrieval within its scope.
    Active,
}

impl Default for LearningPublishTier {
    fn default() -> Self {
        Self::Active
    }
}

/// The daemon decision that produced the current durable learning state.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningPolicyDecision {
    /// The learning was published through an explicit operator action.
    Manual,
    /// The learning was published automatically by daemon policy.
    Automatic,
    /// The daemon refused automatic publication and kept the item for escalation.
    Escalated,
}

/// The current verification state associated with one learning artifact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningVerificationStatus {
    /// No verification has been requested or recorded yet.
    Unverified,
    /// Verification is still in progress.
    Pending,
    /// Verification succeeded against daemon-owned evidence.
    Verified,
    /// Verification failed and the current artifact should not be trusted.
    Failed,
}

impl Default for LearningVerificationStatus {
    fn default() -> Self {
        Self::Unverified
    }
}

/// One compact provenance pointer retained with a learning artifact.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSourceRef {
    /// The originating daemon run when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The originating session when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The originating agent when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The originating input event offset when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_event_offset: Option<u64>,
    /// The originating observation identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<String>,
    /// The originating derivation identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation_id: Option<String>,
}

/// One immutable evidence pointer retained for learning governance and replay.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningEvidenceRef {
    /// The originating daemon run when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// One debug artifact identifier attached to the originating run when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    /// Optional free-form note summarizing why this evidence matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One compact durable learning entry prepared for prompt injection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedContextEntry {
    /// The stable learning identifier.
    pub learning_id: String,
    /// The published learning kind.
    pub kind: LearningKind,
    /// The publication timestamp in milliseconds since the Unix epoch.
    pub published_at_ms: u64,
    /// The compact learned content injected into the prompt.
    pub content: String,
}

/// A bounded set of published learnings prepared for prompt injection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedContextBundle {
    /// The ordered published learnings, most specific first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<LearnedContextEntry>,
    /// Whether older or larger learnings were omitted while packing the bundle.
    #[serde(default)]
    pub truncated: bool,
}

impl LearnedContextBundle {
    /// Returns true when the bundle carries no prompt-visible learning.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Extracts learned context from normalized input metadata.
pub fn learned_context_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<LearnedContextBundle>> {
    metadata
        .get(LEARNED_CONTEXT_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value::<LearnedContextBundle>)
        .transpose()
        .map(|bundle| bundle.filter(|bundle| !bundle.is_empty()))
}

/// Returns metadata with learned context merged in under the stable key.
pub fn metadata_with_learned_context(
    metadata: Value,
    bundle: Option<&LearnedContextBundle>,
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
        LEARNED_CONTEXT_METADATA_KEY.to_string(),
        serde_json::to_value(bundle)?,
    );
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use serde_json::json;

    use super::{
        LEARNED_CONTEXT_METADATA_KEY, LearnedContextBundle, LearnedContextEntry, LearningKind,
        LearningScope, LearningScopeKind, learned_context_from_metadata,
        metadata_with_learned_context,
    };

    fn sample_bundle() -> LearnedContextBundle {
        LearnedContextBundle {
            entries: vec![LearnedContextEntry {
                learning_id: "learning-1".to_string(),
                kind: LearningKind::Fact,
                published_at_ms: 42,
                content: "The workspace prefers compact JSON fixtures.".to_string(),
            }],
            truncated: false,
        }
    }

    #[test]
    fn learned_context_round_trips_through_metadata() {
        let metadata = metadata_with_learned_context(Value::Null, Some(&sample_bundle()))
            .expect("metadata should serialize");
        let bundle = learned_context_from_metadata(&metadata)
            .expect("metadata should deserialize")
            .expect("bundle should exist");
        assert_eq!(bundle, sample_bundle());
    }

    #[test]
    fn learned_context_wraps_non_object_metadata() {
        let metadata = metadata_with_learned_context(json!("legacy"), Some(&sample_bundle()))
            .expect("metadata should serialize");
        assert_eq!(metadata["user_metadata"], json!("legacy"));
        assert!(metadata.get(LEARNED_CONTEXT_METADATA_KEY).is_some());
    }

    #[test]
    fn learned_context_ignores_empty_bundles() {
        let empty = LearnedContextBundle::default();
        let metadata = metadata_with_learned_context(json!({"existing": true}), Some(&empty))
            .expect("metadata should serialize");
        assert_eq!(metadata, json!({"existing": true}));
        assert_eq!(
            learned_context_from_metadata(&json!({
                LEARNED_CONTEXT_METADATA_KEY: {
                    "entries": [],
                    "truncated": false
                }
            }))
            .expect("metadata should deserialize"),
            None
        );
    }

    #[test]
    fn learning_scope_key_is_stable() {
        let scope = LearningScope {
            kind: LearningScopeKind::Project,
            id: "payments".to_string(),
        };
        assert_eq!(scope.scope_key(), "project:payments");
    }
}
