use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use kheish_types::{
    AffinityDrift, AffinityImpression, AffinityStanding, AffinityTabDir, AffinityTabInput,
};
use serde_json::{Value, json};

use super::DaemonToolControlHandle;
use super::helpers::{build_boolean_field, build_string_field, execution_session_id};

fn string_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_drift(raw: Option<&str>) -> Option<AffinityDrift> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        Some("up" | "warmer" | "more" | "higher") => Some(AffinityDrift::Up),
        Some("down" | "cooler" | "less" | "lower") => Some(AffinityDrift::Down),
        _ => None,
    }
}

fn parse_standing(raw: Option<&str>) -> Option<AffinityStanding> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        Some("defer" | "below" | "under") => Some(AffinityStanding::Below),
        Some("lead" | "above" | "over") => Some(AffinityStanding::Above),
        Some("peer" | "equal") => Some(AffinityStanding::Peer),
        _ => None,
    }
}

/// Agent-facing tool that lets an agent evolve its OWN social ledger in the flow
/// of a normal turn — no extra model call. It writes only the caller's ledger, so
/// it scales to hundreds of agents with no shared graph and no cross-session lock.
#[derive(Clone)]
pub(super) struct RememberAboutTool {
    control: DaemonToolControlHandle,
}

impl RememberAboutTool {
    pub(super) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for RememberAboutTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "remember_about".to_string(),
            description: "Privately update how YOU personally read someone you deal with — a colleague, or anyone (a person, not a role). Put their name or id in `peer`. This adjusts only your own impression and colors how you talk to them next time; it never changes what you decide about the work. Use it sparingly, when someone genuinely lands differently than before — warmer, colder, they came through for you, they let you down, you now owe them.".to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field(
                        "peer",
                        "Who this is about — their name or id (e.g. 'Karim', 'operator').",
                        true,
                    ),
                    build_string_field(
                        "display_name",
                        "How you refer to them, if different from `peer`.",
                        false,
                    ),
                    build_string_field(
                        "warmth",
                        "How your warmth toward them shifts: 'up' (warmer) or 'down' (cooler). Omit to leave it.",
                        false,
                    ),
                    build_string_field(
                        "trust",
                        "How your trust in them shifts: 'up' or 'down'. Omit to leave it.",
                        false,
                    ),
                    build_string_field(
                        "standing",
                        "Your standing with them on shared work: 'defer' (you defer to them), 'peer', or 'lead' (they look to you). Omit to leave it.",
                        false,
                    ),
                    build_string_field(
                        "note",
                        "A short, concrete impression in your own words — a fact or quote, not a summary. Replaces the previous note.",
                        false,
                    ),
                    build_string_field(
                        "owe",
                        "Open a tab you now carry: what YOU owe them. Omit if none.",
                        false,
                    ),
                    build_string_field(
                        "owed",
                        "Open a tab in your favor: what THEY owe you. Omit if none.",
                        false,
                    ),
                    build_boolean_field(
                        "settle",
                        "Set true when a prior debt/favor between you is now settled (clears the tab).",
                        false,
                    ),
                ],
            },
            timeout_ms: 10_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let peer = string_field(&input, "peer")
            .ok_or_else(|| anyhow!("peer is required"))?
            .to_string();

        // `owe` (I owe them) takes precedence over `owed` (they owe me) when both
        // are somehow provided; a single edge carries one live tab.
        let tab = if let Some(text) = string_field(&input, "owe") {
            Some(AffinityTabInput {
                text: text.to_string(),
                dir: AffinityTabDir::IOwe,
            })
        } else {
            string_field(&input, "owed").map(|text| AffinityTabInput {
                text: text.to_string(),
                dir: AffinityTabDir::OwedToMe,
            })
        };

        let impression = AffinityImpression {
            peer_id: peer,
            display_name: string_field(&input, "display_name").map(str::to_string),
            warmth: parse_drift(string_field(&input, "warmth")),
            trust: parse_drift(string_field(&input, "trust")),
            standing: parse_standing(string_field(&input, "standing")),
            note: string_field(&input, "note").map(str::to_string),
            tab,
            settle_tab: matches!(input.get("settle"), Some(Value::Bool(true))),
        };

        let edge = self
            .control
            .resolve()?
            .remember_about(session_id, impression)
            .await?;
        // Acknowledge with qualitative state only. The trust/warmth scalars are
        // internal ranking signals and must never echo back to the model as
        // numbers — otherwise they persist, undecayed, in the run transcript.
        Ok(ToolExecutionOutput::json(json!({
            "remembered": edge.display_name,
            "note": edge.note,
            "standing": edge.standing,
            "tab": edge
                .open_tab
                .as_ref()
                .map(|tab| json!({ "dir": tab.dir, "text": tab.text })),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_drift, parse_standing, string_field};
    use kheish_types::{AffinityDrift, AffinityStanding};
    use serde_json::json;

    #[test]
    fn parse_drift_maps_synonyms_and_ignores_noise() {
        assert_eq!(parse_drift(Some("up")), Some(AffinityDrift::Up));
        assert_eq!(parse_drift(Some("Warmer")), Some(AffinityDrift::Up));
        assert_eq!(parse_drift(Some("down")), Some(AffinityDrift::Down));
        assert_eq!(parse_drift(Some("cooler")), Some(AffinityDrift::Down));
        assert_eq!(parse_drift(Some("sideways")), None);
        assert_eq!(parse_drift(None), None);
    }

    #[test]
    fn parse_standing_maps_roles_case_insensitively() {
        assert_eq!(parse_standing(Some("defer")), Some(AffinityStanding::Below));
        assert_eq!(parse_standing(Some("LEAD")), Some(AffinityStanding::Above));
        assert_eq!(parse_standing(Some("peer")), Some(AffinityStanding::Peer));
        assert_eq!(parse_standing(Some("boss")), None);
    }

    #[test]
    fn string_field_trims_and_drops_empty() {
        let input = json!({ "peer": "  Karim  ", "note": "   " });
        assert_eq!(string_field(&input, "peer"), Some("Karim"));
        assert_eq!(string_field(&input, "note"), None);
        assert_eq!(string_field(&input, "missing"), None);
    }
}
