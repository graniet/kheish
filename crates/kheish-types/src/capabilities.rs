use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// One declarative persona-owned inline skill assignment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaSkillAssignment {
    /// Stable skill name resolved against the daemon skill registry.
    pub name: String,
    /// Optional free-form arguments passed through to the skill template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
}

/// One compact visibility policy applied to skills and MCP surfaces.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityScope {
    /// Optional skill allow-list. When empty, every skill remains eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_allow: Vec<String>,
    /// Optional skill deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_deny: Vec<String>,
    /// Optional MCP server allow-list. When empty, every server remains eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server_allow: Vec<String>,
    /// Optional MCP server deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server_deny: Vec<String>,
    /// Optional qualified MCP tool allow-list. When empty, every tool remains eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_tool_allow: Vec<String>,
    /// Optional qualified MCP tool deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_tool_deny: Vec<String>,
}

impl CapabilityScope {
    /// Returns true when the scope does not constrain any capability families.
    pub fn is_empty(&self) -> bool {
        self.skill_allow.is_empty()
            && self.skill_deny.is_empty()
            && self.mcp_server_allow.is_empty()
            && self.mcp_server_deny.is_empty()
            && self.mcp_tool_allow.is_empty()
            && self.mcp_tool_deny.is_empty()
    }

    /// Returns a normalized copy with trimmed, sorted, deduplicated entries.
    pub fn normalized(&self) -> Self {
        Self {
            skill_allow: normalize_entries(&self.skill_allow),
            skill_deny: normalize_entries(&self.skill_deny),
            mcp_server_allow: normalize_entries(&self.mcp_server_allow),
            mcp_server_deny: normalize_entries(&self.mcp_server_deny),
            mcp_tool_allow: normalize_entries(&self.mcp_tool_allow),
            mcp_tool_deny: normalize_entries(&self.mcp_tool_deny),
        }
    }

    /// Returns whether one skill name remains visible under this scope.
    pub fn allows_skill(&self, name: &str) -> bool {
        allows_entry(&self.skill_allow, &self.skill_deny, name)
    }

    /// Returns whether one MCP server remains visible under this scope.
    pub fn allows_mcp_server(&self, server: &str) -> bool {
        allows_entry(&self.mcp_server_allow, &self.mcp_server_deny, server)
    }

    /// Returns whether one qualified MCP tool remains visible under this scope.
    pub fn allows_mcp_tool(&self, qualified_name: &str, server: Option<&str>) -> bool {
        self.allows_mcp_server(server.unwrap_or_default())
            && allows_entry(&self.mcp_tool_allow, &self.mcp_tool_deny, qualified_name)
    }

    /// Returns whether one MCP helper tool remains visible under this scope.
    pub fn allows_mcp_helper_tool(&self, name: &str) -> bool {
        allows_entry(&self.mcp_tool_allow, &self.mcp_tool_deny, name)
    }

    /// Returns one scope that is no wider than either input scope.
    pub fn restrict_with(&self, narrower: &Self) -> Self {
        let base = self.normalized();
        let narrower = narrower.normalized();
        Self {
            skill_allow: intersect_allow_lists(&base.skill_allow, &narrower.skill_allow),
            skill_deny: union_lists(&base.skill_deny, &narrower.skill_deny),
            mcp_server_allow: intersect_allow_lists(
                &base.mcp_server_allow,
                &narrower.mcp_server_allow,
            ),
            mcp_server_deny: union_lists(&base.mcp_server_deny, &narrower.mcp_server_deny),
            mcp_tool_allow: intersect_allow_lists(&base.mcp_tool_allow, &narrower.mcp_tool_allow),
            mcp_tool_deny: union_lists(&base.mcp_tool_deny, &narrower.mcp_tool_deny),
        }
    }
}

/// One compact credential-visibility policy applied to route and connector secret usage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialScope {
    /// Optional route allow-list. When empty, every route remains eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_allow: Vec<String>,
    /// Optional route deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_deny: Vec<String>,
    /// Optional connector allow-list. When empty, every connector remains eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connector_allow: Vec<String>,
    /// Optional connector deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connector_deny: Vec<String>,
    /// Optional connector credential allow-list keyed as `connector:ENV_KEY`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connector_credential_allow: Vec<String>,
    /// Optional connector credential deny-list keyed as `connector:ENV_KEY`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connector_credential_deny: Vec<String>,
    /// Optional MCP server allow-list. When empty, every server remains eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server_allow: Vec<String>,
    /// Optional MCP server deny-list applied after the allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server_deny: Vec<String>,
}

impl CredentialScope {
    /// Returns the default scope for delegated agents that did not request credential access.
    ///
    /// Model routes remain eligible so ordinary sub-agents can still run, but connector and MCP
    /// credential-bearing surfaces require an explicit delegated scope.
    pub fn deny_delegated_non_route_credentials() -> Self {
        Self {
            connector_deny: vec!["*".to_string()],
            connector_credential_deny: vec!["*".to_string()],
            mcp_server_deny: vec!["*".to_string()],
            ..Self::default()
        }
    }

    /// Returns true when the scope does not constrain any credential-bearing families.
    pub fn is_empty(&self) -> bool {
        self.route_allow.is_empty()
            && self.route_deny.is_empty()
            && self.connector_allow.is_empty()
            && self.connector_deny.is_empty()
            && self.connector_credential_allow.is_empty()
            && self.connector_credential_deny.is_empty()
            && self.mcp_server_allow.is_empty()
            && self.mcp_server_deny.is_empty()
    }

    /// Returns a normalized copy with trimmed, sorted, deduplicated entries.
    pub fn normalized(&self) -> Self {
        Self {
            route_allow: normalize_entries(&self.route_allow),
            route_deny: normalize_entries(&self.route_deny),
            connector_allow: normalize_entries(&self.connector_allow),
            connector_deny: normalize_entries(&self.connector_deny),
            connector_credential_allow: normalize_entries(&self.connector_credential_allow),
            connector_credential_deny: normalize_entries(&self.connector_credential_deny),
            mcp_server_allow: normalize_entries(&self.mcp_server_allow),
            mcp_server_deny: normalize_entries(&self.mcp_server_deny),
        }
    }

    /// Returns whether one route remains eligible under this scope.
    pub fn allows_route(&self, route_id: &str) -> bool {
        allows_entry(&self.route_allow, &self.route_deny, route_id)
    }

    /// Returns whether one connector remains eligible under this scope.
    pub fn allows_connector(&self, connector: &str) -> bool {
        allows_entry(&self.connector_allow, &self.connector_deny, connector)
    }

    /// Returns whether one connector credential remains eligible under this scope.
    pub fn allows_connector_credential(&self, connector: &str, env_key: &str) -> bool {
        self.allows_connector(connector)
            && if self.connector_credential_allow.is_empty()
                && self.connector_credential_deny.is_empty()
            {
                !connector_credentials_default_to_none(self)
            } else {
                allows_entry(
                    &self.connector_credential_allow,
                    &self.connector_credential_deny,
                    &format!("{connector}:{env_key}"),
                )
            }
    }

    /// Returns whether one MCP server remains eligible under this scope.
    pub fn allows_mcp_server(&self, server: &str) -> bool {
        allows_entry(&self.mcp_server_allow, &self.mcp_server_deny, server)
    }

    /// Returns one scope that is no wider than either input scope.
    pub fn restrict_with(&self, narrower: &Self) -> Self {
        let base = self.normalized();
        let narrower = narrower.normalized();
        Self {
            route_allow: intersect_allow_lists(&base.route_allow, &narrower.route_allow),
            route_deny: union_lists(&base.route_deny, &narrower.route_deny),
            connector_allow: intersect_allow_lists(
                &base.connector_allow,
                &narrower.connector_allow,
            ),
            connector_deny: union_lists(&base.connector_deny, &narrower.connector_deny),
            connector_credential_allow: intersect_connector_credential_allow_lists(
                &base, &narrower,
            ),
            connector_credential_deny: union_lists(
                &base.connector_credential_deny,
                &narrower.connector_credential_deny,
            ),
            mcp_server_allow: intersect_allow_lists(
                &base.mcp_server_allow,
                &narrower.mcp_server_allow,
            ),
            mcp_server_deny: union_lists(&base.mcp_server_deny, &narrower.mcp_server_deny),
        }
    }
}

fn normalize_entries(entries: &[String]) -> Vec<String> {
    let normalized = entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if normalized.iter().any(|entry| entry == "*") {
        return vec!["*".to_string()];
    }
    normalized
}

fn connector_credentials_default_to_none(scope: &CredentialScope) -> bool {
    (!scope.connector_allow.is_empty() || !scope.connector_deny.is_empty())
        && scope.connector_credential_allow.is_empty()
        && scope.connector_credential_deny.is_empty()
}

/// Returns whether one concrete entry is covered by one allow-list.
///
/// Empty allow-lists stay permissive, while `*` covers every concrete entry.
pub fn allow_list_allows_entry(allow: &[String], entry: &str) -> bool {
    allow.is_empty()
        || allow
            .iter()
            .any(|candidate| entry_matches(candidate, entry))
}

fn allows_entry(allow: &[String], deny: &[String], entry: &str) -> bool {
    let allowed = allow_list_allows_entry(allow, entry);
    allowed && !deny.iter().any(|candidate| entry_matches(candidate, entry))
}

fn entry_matches(candidate: &str, entry: &str) -> bool {
    candidate == "*" || candidate == entry
}

fn intersect_allow_lists(left: &[String], right: &[String]) -> Vec<String> {
    if left.is_empty() {
        return right.to_vec();
    }
    if right.is_empty() {
        return left.to_vec();
    }
    if left.iter().any(|entry| entry == "*") {
        return right.to_vec();
    }
    if right.iter().any(|entry| entry == "*") {
        return left.to_vec();
    }
    let right = right.iter().collect::<BTreeSet<_>>();
    left.iter()
        .filter(|entry| right.contains(entry))
        .cloned()
        .collect()
}

fn union_lists(left: &[String], right: &[String]) -> Vec<String> {
    left.iter()
        .chain(right.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn intersect_connector_credential_allow_lists(
    left: &CredentialScope,
    right: &CredentialScope,
) -> Vec<String> {
    if connector_credentials_default_to_none(left) || connector_credentials_default_to_none(right) {
        return Vec::new();
    }
    intersect_allow_lists(
        &left.connector_credential_allow,
        &right.connector_credential_allow,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_trims_deduplicates_and_sorts_entries() {
        let scope = CapabilityScope {
            skill_allow: vec![
                " beta ".to_string(),
                "alpha".to_string(),
                "alpha".to_string(),
            ],
            skill_deny: vec![" ".to_string()],
            ..CapabilityScope::default()
        };

        let normalized = scope.normalized();
        assert_eq!(
            normalized.skill_allow,
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert!(normalized.skill_deny.is_empty());
    }

    #[test]
    fn restrict_with_intersects_allowlists_and_unions_denylists() {
        let base = CapabilityScope {
            skill_allow: vec!["alpha".to_string(), "beta".to_string()],
            mcp_tool_deny: vec!["mcp__blocked__tool".to_string()],
            ..CapabilityScope::default()
        };
        let narrower = CapabilityScope {
            skill_allow: vec!["beta".to_string(), "gamma".to_string()],
            mcp_tool_deny: vec!["mcp__other__tool".to_string()],
            ..CapabilityScope::default()
        };

        let restricted = base.restrict_with(&narrower);
        assert_eq!(restricted.skill_allow, vec!["beta".to_string()]);
        assert_eq!(
            restricted.mcp_tool_deny,
            vec![
                "mcp__blocked__tool".to_string(),
                "mcp__other__tool".to_string()
            ]
        );
    }

    #[test]
    fn allows_mcp_tool_requires_server_and_tool_visibility() {
        let scope = CapabilityScope {
            mcp_server_allow: vec!["linear".to_string()],
            mcp_tool_deny: vec!["mcp__linear__delete_issue".to_string()],
            ..CapabilityScope::default()
        };

        assert!(scope.allows_mcp_tool("mcp__linear__get_issue", Some("linear")));
        assert!(!scope.allows_mcp_tool("mcp__linear__delete_issue", Some("linear")));
        assert!(!scope.allows_mcp_tool("mcp__other__get_issue", Some("other")));
    }

    #[test]
    fn allows_mcp_helper_tool_uses_tool_allow_and_deny_only() {
        let scope = CapabilityScope {
            mcp_server_allow: vec!["openaiDeveloperDocs".to_string()],
            mcp_tool_deny: vec!["read_mcp_resource".to_string()],
            ..CapabilityScope::default()
        };

        assert!(scope.allows_mcp_helper_tool("list_mcp_resources"));
        assert!(!scope.allows_mcp_helper_tool("read_mcp_resource"));
    }

    #[test]
    fn credential_scope_normalizes_and_restricts_entries() {
        let base = CredentialScope {
            route_allow: vec![" openai ".to_string(), "anthropic".to_string()],
            connector_credential_deny: vec!["slack:BOT_TOKEN".to_string()],
            ..CredentialScope::default()
        };
        let narrower = CredentialScope {
            route_allow: vec!["openai".to_string(), "google".to_string()],
            connector_credential_deny: vec!["slack:SIGNING_SECRET".to_string()],
            ..CredentialScope::default()
        };

        let restricted = base.restrict_with(&narrower);
        assert_eq!(restricted.route_allow, vec!["openai".to_string()]);
        assert_eq!(
            restricted.connector_credential_deny,
            vec![
                "slack:BOT_TOKEN".to_string(),
                "slack:SIGNING_SECRET".to_string()
            ]
        );
    }

    #[test]
    fn credential_scope_checks_connector_credentials() {
        let scope = CredentialScope {
            connector_allow: vec!["slack".to_string()],
            connector_credential_allow: vec!["slack:SIGNING_SECRET".to_string()],
            connector_credential_deny: vec!["slack:BOT_TOKEN".to_string()],
            ..CredentialScope::default()
        };

        assert!(scope.allows_connector_credential("slack", "SIGNING_SECRET"));
        assert!(!scope.allows_connector_credential("slack", "BOT_TOKEN"));
        assert!(!scope.allows_connector_credential("github", "TOKEN"));
    }

    #[test]
    fn credential_scope_requires_explicit_credential_allow_when_connectors_are_scoped() {
        let scope = CredentialScope {
            connector_allow: vec!["slack".to_string()],
            ..CredentialScope::default()
        };

        assert!(!scope.allows_connector_credential("slack", "BOT_TOKEN"));
        assert!(!scope.allows_connector_credential("slack", "SIGNING_SECRET"));
    }

    #[test]
    fn delegated_default_scope_denies_connector_and_mcp_credentials_but_keeps_routes() {
        let scope = CredentialScope::deny_delegated_non_route_credentials();

        assert!(scope.allows_route("openai"));
        assert!(!scope.allows_connector("github"));
        assert!(!scope.allows_connector_credential("github", "app_token"));
        assert!(!scope.allows_mcp_server("github"));
    }

    #[test]
    fn credential_scope_restrict_with_does_not_reenable_hidden_connector_credentials() {
        let parent = CredentialScope {
            connector_allow: vec!["slack".to_string()],
            ..CredentialScope::default()
        };
        let child = CredentialScope {
            connector_allow: vec!["slack".to_string()],
            connector_credential_allow: vec!["slack:bot_token".to_string()],
            ..CredentialScope::default()
        };

        let restricted = parent.restrict_with(&child);
        assert!(restricted.connector_credential_allow.is_empty());
        assert!(!restricted.allows_connector_credential("slack", "bot_token"));
    }

    #[test]
    fn normalize_entries_collapses_redundant_wildcards() {
        let scope = CredentialScope {
            route_allow: vec!["openai".to_string(), "*".to_string()],
            connector_deny: vec!["*".to_string(), "github".to_string()],
            ..CredentialScope::default()
        };

        let normalized = scope.normalized();
        assert_eq!(normalized.route_allow, vec!["*".to_string()]);
        assert_eq!(normalized.connector_deny, vec!["*".to_string()]);
    }

    #[test]
    fn capability_scope_restrict_with_honors_parent_wildcards() {
        let parent = CapabilityScope {
            skill_allow: vec!["*".to_string()],
            mcp_server_allow: vec!["*".to_string()],
            mcp_tool_allow: vec!["*".to_string()],
            ..CapabilityScope::default()
        };
        let child = CapabilityScope {
            skill_allow: vec!["alpha".to_string()],
            mcp_server_allow: vec!["github".to_string()],
            mcp_tool_allow: vec!["mcp__github__search_code".to_string()],
            ..CapabilityScope::default()
        };

        let restricted = parent.restrict_with(&child);
        assert_eq!(restricted.skill_allow, vec!["alpha".to_string()]);
        assert_eq!(restricted.mcp_server_allow, vec!["github".to_string()]);
        assert_eq!(
            restricted.mcp_tool_allow,
            vec!["mcp__github__search_code".to_string()]
        );
    }

    #[test]
    fn credential_scope_restrict_with_honors_parent_wildcards() {
        let parent = CredentialScope {
            route_allow: vec!["*".to_string()],
            connector_allow: vec!["*".to_string()],
            connector_credential_allow: vec!["*".to_string()],
            mcp_server_allow: vec!["*".to_string()],
            ..CredentialScope::default()
        };
        let child = CredentialScope {
            route_allow: vec!["openai".to_string()],
            connector_allow: vec!["github".to_string()],
            connector_credential_allow: vec!["github:app_token".to_string()],
            mcp_server_allow: vec!["github".to_string()],
            ..CredentialScope::default()
        };

        let restricted = parent.restrict_with(&child);
        assert_eq!(restricted.route_allow, vec!["openai".to_string()]);
        assert_eq!(restricted.connector_allow, vec!["github".to_string()]);
        assert_eq!(
            restricted.connector_credential_allow,
            vec!["github:app_token".to_string()]
        );
        assert_eq!(restricted.mcp_server_allow, vec!["github".to_string()]);
    }

    #[test]
    fn credential_scope_restrict_with_does_not_allow_child_wildcards_to_widen_parent() {
        let parent = CredentialScope {
            route_allow: vec!["openai".to_string()],
            mcp_server_allow: vec!["github".to_string()],
            ..CredentialScope::default()
        };
        let child = CredentialScope {
            route_allow: vec!["*".to_string()],
            mcp_server_allow: vec!["*".to_string()],
            ..CredentialScope::default()
        };

        let restricted = parent.restrict_with(&child);
        assert_eq!(restricted.route_allow, vec!["openai".to_string()]);
        assert_eq!(restricted.mcp_server_allow, vec!["github".to_string()]);
        assert!(restricted.allows_route("openai"));
        assert!(!restricted.allows_route("anthropic"));
        assert!(restricted.allows_mcp_server("github"));
        assert!(!restricted.allows_mcp_server("linear"));
    }

    #[test]
    fn credential_scope_restrict_with_keeps_parent_denies_under_wildcard_narrowing() {
        let parent = CredentialScope {
            route_allow: vec!["*".to_string()],
            route_deny: vec!["anthropic".to_string()],
            ..CredentialScope::default()
        };
        let child = CredentialScope {
            route_allow: vec!["anthropic".to_string()],
            ..CredentialScope::default()
        };

        let restricted = parent.restrict_with(&child);
        assert_eq!(restricted.route_allow, vec!["anthropic".to_string()]);
        assert_eq!(restricted.route_deny, vec!["anthropic".to_string()]);
        assert!(!restricted.allows_route("anthropic"));
    }
}
