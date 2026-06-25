use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable metadata key used to persist session skill state.
pub const SESSION_SKILLS_STATE_METADATA_KEY: &str = "session_skills_state";
/// Stable metadata key used to persist the daemon-projected session-visible skill names.
pub const SESSION_VISIBLE_SKILLS_METADATA_KEY: &str = "session_visible_skills";

/// Execution mode requested by one reusable skill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillExecutionContext {
    /// Inject the skill into the current agent context.
    #[default]
    Inline,
    /// Execute the skill inside a dedicated child agent.
    Fork,
}

/// One activated skill snapshot persisted with a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSkillSnapshot {
    /// Stable skill name.
    pub name: String,
    /// Short description surfaced to operators and prompts.
    pub description: String,
    /// Optional when-to-use guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Optional version string declared by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Canonical skill markdown path.
    pub skill_path: String,
    /// Canonical skill directory.
    pub skill_root: String,
    /// Content digest pinned when the skill was activated.
    pub digest: String,
    /// Optional free-form arguments captured at activation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// The execution mode used for this skill snapshot.
    #[serde(default)]
    pub context: SkillExecutionContext,
    /// Preferred tools declared by the skill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Tools the skill asked to avoid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    /// Optional agent profile requested by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// Optional provider override declared by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional primary model override declared by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional fallback model override declared by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// Human-readable activation reason recorded by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_reason: Option<String>,
    /// Rendered instructions pinned at activation time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
}

/// Session-scoped skills state persisted with runtime metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSkillsState {
    /// The currently active inline skills for this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<ActiveSkillSnapshot>,
}

/// Decodes persisted session skill state from metadata.
pub fn session_skills_state_from_metadata(
    metadata: &Value,
) -> serde_json::Result<SessionSkillsState> {
    metadata
        .get(SESSION_SKILLS_STATE_METADATA_KEY)
        .cloned()
        .map(serde_json::from_value)
        .unwrap_or_else(|| Ok(SessionSkillsState::default()))
}

/// Returns metadata with session skill state merged under the stable key.
pub fn metadata_with_session_skills_state(
    metadata: Value,
    state: &SessionSkillsState,
) -> serde_json::Result<Value> {
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
        SESSION_SKILLS_STATE_METADATA_KEY.to_string(),
        serde_json::to_value(state)?,
    );
    Ok(Value::Object(object))
}

/// Decodes the daemon-projected session-visible skill names from metadata.
pub fn session_visible_skills_from_metadata(
    metadata: &Value,
) -> serde_json::Result<Option<Vec<String>>> {
    match metadata.get(SESSION_VISIBLE_SKILLS_METADATA_KEY) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => serde_json::from_value::<Vec<String>>(value.clone()).map(Some),
    }
    .map(|skills| {
        skills.map(|skills| {
            let mut deduped = Vec::with_capacity(skills.len());
            for skill in skills {
                let normalized = skill.trim();
                if normalized.is_empty()
                    || deduped
                        .iter()
                        .any(|existing: &String| existing == normalized)
                {
                    continue;
                }
                deduped.push(normalized.to_string());
            }
            deduped
        })
    })
}

/// Returns metadata with the daemon-projected session-visible skills merged under the stable key.
pub fn metadata_with_session_visible_skills(
    metadata: Value,
    skills: &[String],
) -> serde_json::Result<Value> {
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
        SESSION_VISIBLE_SKILLS_METADATA_KEY.to_string(),
        serde_json::to_value(skills)?,
    );
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        SESSION_VISIBLE_SKILLS_METADATA_KEY, metadata_with_session_visible_skills,
        session_visible_skills_from_metadata,
    };

    #[test]
    fn session_visible_skills_round_trip_through_metadata() {
        let metadata = metadata_with_session_visible_skills(
            serde_json::Value::Null,
            &["alpha".to_string(), "beta".to_string()],
        )
        .expect("metadata should serialize");
        let decoded = session_visible_skills_from_metadata(&metadata)
            .expect("metadata should deserialize")
            .expect("visible skills should exist");
        assert_eq!(decoded, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn session_visible_skills_wrap_non_object_metadata() {
        let metadata = metadata_with_session_visible_skills(
            json!("legacy"),
            &["alpha".to_string(), "beta".to_string()],
        )
        .expect("metadata should serialize");
        assert_eq!(metadata["user_metadata"], json!("legacy"));
        assert_eq!(
            metadata[SESSION_VISIBLE_SKILLS_METADATA_KEY],
            json!(["alpha", "beta"])
        );
    }

    #[test]
    fn session_visible_skills_are_trimmed_and_deduped() {
        let decoded = session_visible_skills_from_metadata(&json!({
            SESSION_VISIBLE_SKILLS_METADATA_KEY: ["alpha", " alpha ", "", "beta", "alpha"]
        }))
        .expect("metadata should deserialize")
        .expect("visible skills should exist");
        assert_eq!(decoded, vec!["alpha".to_string(), "beta".to_string()]);
    }
}
