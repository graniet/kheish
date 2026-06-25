//! Built-in MCP catalog entries and profile expansion.

use std::collections::{BTreeMap, BTreeSet};
use std::env;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::{McpHttpAuth, McpServerConfig, McpServerTransport};

/// Operational maturity of one built-in catalog entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCatalogStatus {
    /// Kheish can render and start this entry directly.
    Supported,
    /// The server is documented for operators but is not started by built-in profiles yet.
    /// Common reasons are interactive OAuth client setup or unpinned local executables.
    CatalogOnly,
}

/// Authentication shape expected by one MCP server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCatalogAuthKind {
    None,
    OptionalBearer,
    Bearer,
    OAuth,
    OAuthClientApp,
}

/// Risk classification used for operator display and default profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCatalogRisk {
    ReadOnlyDocs,
    ReadMostlyWorkspace,
    AccountReadWrite,
    BrowserAutomation,
    InfrastructureMutation,
}

/// One built-in catalog entry as shown by the CLI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCatalogEntryView {
    pub id: String,
    pub server_name: String,
    pub display_name: String,
    pub description: String,
    pub category: String,
    pub profiles: Vec<String>,
    pub status: McpCatalogStatus,
    pub risk: McpCatalogRisk,
    pub auth: McpCatalogAuthKind,
    pub transport: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_env: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_secret_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    pub docs_url: String,
}

/// Daemon-provided secret values available to built-in catalog entries.
#[derive(Clone, PartialEq, Eq)]
pub struct McpResolvedSecrets {
    /// Secret values keyed by daemon auth-store slot reference.
    pub secret_values: BTreeMap<String, String>,
    /// Secret refs explicitly revoked in the daemon auth broker.
    pub revoked_secret_refs: BTreeSet<String>,
    /// Whether catalog entries may fall back to their documented environment variables.
    pub allow_env_fallback: bool,
}

impl Default for McpResolvedSecrets {
    fn default() -> Self {
        Self {
            secret_values: BTreeMap::new(),
            revoked_secret_refs: BTreeSet::new(),
            allow_env_fallback: true,
        }
    }
}

impl std::fmt::Debug for McpResolvedSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpResolvedSecrets")
            .field(
                "secret_values",
                &format_args!("{} redacted", self.secret_values.len()),
            )
            .field("revoked_secret_refs", &self.revoked_secret_refs.len())
            .field("allow_env_fallback", &self.allow_env_fallback)
            .finish()
    }
}

/// Returns the daemon auth-store slot used for one catalog credential.
pub fn catalog_credential_secret_ref(entry_id: &str, credential_env: &str) -> String {
    format!("mcp.{entry_id}.{credential_env}")
}

/// One built-in MCP profile as shown by the CLI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCatalogProfileView {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub entry_ids: Vec<String>,
}

/// Normalizes and validates one built-in MCP profile name.
pub fn normalize_catalog_profile_name(profile: &str) -> Result<String> {
    let normalized = profile.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(anyhow!("MCP catalog profile cannot be empty"));
    }
    ensure_known_profile(&normalized)?;
    Ok(normalized)
}

#[derive(Clone, Copy)]
struct McpCatalogEntry {
    id: &'static str,
    server_name: &'static str,
    display_name: &'static str,
    description: &'static str,
    category: &'static str,
    profiles: &'static [&'static str],
    status: McpCatalogStatus,
    risk: McpCatalogRisk,
    auth: McpCatalogAuthKind,
    transport: McpCatalogTransport,
    credential_env: &'static [&'static str],
    enabled_tools: &'static [&'static str],
    disabled_tools: &'static [&'static str],
    docs_url: &'static str,
}

#[derive(Clone, Copy)]
enum McpCatalogTransport {
    StreamableHttp {
        url: &'static str,
    },
    Stdio {
        command: &'static str,
        args: &'static [&'static str],
        env: &'static [(&'static str, &'static str)],
    },
}

#[derive(Clone, Copy)]
struct McpCatalogProfile {
    id: &'static str,
    display_name: &'static str,
    description: &'static str,
}

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_TOOL_TIMEOUT_MS: u64 = 120_000;

const PROFILES: &[McpCatalogProfile] = &[
    McpCatalogProfile {
        id: "docs",
        display_name: "Documentation",
        description: "Read-only vendor documentation MCP servers with no account credentials.",
    },
    McpCatalogProfile {
        id: "repo",
        display_name: "Repository",
        description: "Repository, issue, pull request, and source-control context.",
    },
    McpCatalogProfile {
        id: "planning",
        display_name: "Planning",
        description: "Product planning and work-management context.",
    },
    McpCatalogProfile {
        id: "knowledge",
        display_name: "Knowledge",
        description: "Workspace knowledge bases and project documentation.",
    },
    McpCatalogProfile {
        id: "communication",
        display_name: "Communication",
        description: "Team communication systems.",
    },
    McpCatalogProfile {
        id: "browser-ui",
        display_name: "Browser UI",
        description: "Browser automation and UI inspection.",
    },
    McpCatalogProfile {
        id: "observability",
        display_name: "Observability",
        description: "Production debugging, events, logs, and monitoring context.",
    },
    McpCatalogProfile {
        id: "data-dev",
        display_name: "Data Development",
        description: "Development database and backend data-plane tools.",
    },
    McpCatalogProfile {
        id: "deploy-infra",
        display_name: "Deployment And Infrastructure",
        description: "Cloud, deployment, and infrastructure management surfaces.",
    },
    McpCatalogProfile {
        id: "business",
        display_name: "Business",
        description: "Billing, payment, design, and business operations surfaces.",
    },
];

const CATALOG: &[McpCatalogEntry] = &[
    McpCatalogEntry {
        id: "openai-docs",
        server_name: "openaiDeveloperDocs",
        display_name: "OpenAI Developer Docs",
        description: "Read-only OpenAI and Codex developer documentation.",
        category: "documentation",
        profiles: &["docs"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::ReadOnlyDocs,
        auth: McpCatalogAuthKind::None,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://developers.openai.com/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://platform.openai.com/docs/docs-mcp",
    },
    McpCatalogEntry {
        id: "microsoft-learn",
        server_name: "microsoftLearn",
        display_name: "Microsoft Learn",
        description: "Read-only Microsoft Learn documentation and code samples.",
        category: "documentation",
        profiles: &["docs"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::ReadOnlyDocs,
        auth: McpCatalogAuthKind::None,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://learn.microsoft.com/api/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://learn.microsoft.com/en-us/training/support/mcp",
    },
    McpCatalogEntry {
        id: "cloudflare-docs",
        server_name: "cloudflareDocs",
        display_name: "Cloudflare Docs",
        description: "Read-only Cloudflare product documentation.",
        category: "documentation",
        profiles: &["docs", "deploy-infra"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::ReadOnlyDocs,
        auth: McpCatalogAuthKind::None,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://docs.mcp.cloudflare.com/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://developers.cloudflare.com/agents/model-context-protocol/mcp-servers-for-cloudflare/",
    },
    McpCatalogEntry {
        id: "cloudflare-agents-docs",
        server_name: "cloudflareAgentsDocs",
        display_name: "Cloudflare Agents SDK Docs",
        description: "Read-only Cloudflare Agents SDK documentation.",
        category: "documentation",
        profiles: &["docs", "deploy-infra"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::ReadOnlyDocs,
        auth: McpCatalogAuthKind::None,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://agents.cloudflare.com/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://developers.cloudflare.com/agents/model-context-protocol/mcp-servers-for-cloudflare/",
    },
    McpCatalogEntry {
        id: "github",
        server_name: "github",
        display_name: "GitHub",
        description: "Official GitHub MCP server for repository, issue, pull request, and user context.",
        category: "repository",
        profiles: &["repo"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::ReadMostlyWorkspace,
        auth: McpCatalogAuthKind::Bearer,
        transport: McpCatalogTransport::Stdio {
            command: "docker",
            args: &[
                "run",
                "-i",
                "--rm",
                "-e",
                "GITHUB_PERSONAL_ACCESS_TOKEN",
                "-e",
                "GITHUB_TOOLSETS",
                "ghcr.io/github/github-mcp-server",
            ],
            env: &[(
                "GITHUB_TOOLSETS",
                "context,repos,issues,pull_requests,users",
            )],
        },
        credential_env: &["GITHUB_PERSONAL_ACCESS_TOKEN"],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://github.com/github/github-mcp-server",
    },
    McpCatalogEntry {
        id: "gitlab",
        server_name: "gitlab",
        display_name: "GitLab",
        description: "GitLab MCP server for projects, issues, merge requests, and GitLab API context.",
        category: "repository",
        profiles: &["repo"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::ReadMostlyWorkspace,
        auth: McpCatalogAuthKind::OptionalBearer,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://gitlab.com/api/v4/mcp",
        },
        credential_env: &["GITLAB_TOKEN"],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://docs.gitlab.com/user/gitlab_duo/model_context_protocol/mcp_server/",
    },
    McpCatalogEntry {
        id: "linear",
        server_name: "linear",
        display_name: "Linear",
        description: "Linear MCP server for issues, projects, initiatives, comments, and product planning.",
        category: "planning",
        profiles: &["planning"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::AccountReadWrite,
        auth: McpCatalogAuthKind::OptionalBearer,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.linear.app/mcp",
        },
        credential_env: &["LINEAR_API_KEY"],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://linear.app/docs/mcp",
    },
    McpCatalogEntry {
        id: "notion",
        server_name: "notion",
        display_name: "Notion",
        description: "Hosted Notion MCP server for workspace search, pages, and databases.",
        category: "knowledge",
        profiles: &["knowledge"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::AccountReadWrite,
        auth: McpCatalogAuthKind::OAuth,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.notion.com/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://developers.notion.com/docs/get-started-with-mcp",
    },
    McpCatalogEntry {
        id: "atlassian",
        server_name: "atlassian",
        display_name: "Atlassian Rovo",
        description: "Atlassian Rovo MCP server for Jira, Confluence, and Compass.",
        category: "planning",
        profiles: &["planning", "knowledge"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::AccountReadWrite,
        auth: McpCatalogAuthKind::OAuth,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.atlassian.com/v1/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://support.atlassian.com/atlassian-rovo-mcp-server/docs/getting-started-with-the-atlassian-remote-mcp-server/",
    },
    McpCatalogEntry {
        id: "slack",
        server_name: "slack",
        display_name: "Slack",
        description: "Slack MCP server for search, messages, canvases, channels, and user information.",
        category: "communication",
        profiles: &["communication"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::AccountReadWrite,
        auth: McpCatalogAuthKind::OAuthClientApp,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.slack.com/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://docs.slack.dev/ai/slack-mcp-server/",
    },
    McpCatalogEntry {
        id: "playwright",
        server_name: "playwright",
        display_name: "Playwright",
        description: "Microsoft Playwright MCP server for browser automation and UI inspection.",
        category: "browser",
        profiles: &["browser-ui"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::BrowserAutomation,
        auth: McpCatalogAuthKind::None,
        transport: McpCatalogTransport::Stdio {
            command: "npx",
            args: &["-y", "@playwright/mcp@latest"],
            env: &[],
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://github.com/microsoft/playwright-mcp",
    },
    McpCatalogEntry {
        id: "sentry",
        server_name: "sentry",
        display_name: "Sentry",
        description: "Sentry MCP server for issue debugging, events, and project observability.",
        category: "observability",
        profiles: &["observability"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::AccountReadWrite,
        auth: McpCatalogAuthKind::Bearer,
        transport: McpCatalogTransport::Stdio {
            command: "npx",
            args: &["-y", "@sentry/mcp-server@latest"],
            env: &[],
        },
        credential_env: &["SENTRY_ACCESS_TOKEN"],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://github.com/getsentry/sentry-mcp",
    },
    McpCatalogEntry {
        id: "neon",
        server_name: "neon",
        display_name: "Neon",
        description: "Neon MCP server for development Postgres projects, branches, SQL, and migrations.",
        category: "data",
        profiles: &["data-dev"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::InfrastructureMutation,
        auth: McpCatalogAuthKind::OptionalBearer,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.neon.tech/mcp",
        },
        credential_env: &["NEON_API_KEY"],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://neon.com/docs/ai/neon-mcp-server",
    },
    McpCatalogEntry {
        id: "vercel",
        server_name: "vercel",
        display_name: "Vercel",
        description: "Vercel MCP server for documentation, projects, deployments, and logs.",
        category: "deployment",
        profiles: &["deploy-infra"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::InfrastructureMutation,
        auth: McpCatalogAuthKind::OAuth,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.vercel.com",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://vercel.com/docs/ai-resources/vercel-mcp",
    },
    McpCatalogEntry {
        id: "cloudflare-api",
        server_name: "cloudflareApi",
        display_name: "Cloudflare API",
        description: "Cloudflare API MCP server using two codemode tools for broad Cloudflare account access.",
        category: "infrastructure",
        profiles: &["deploy-infra"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::InfrastructureMutation,
        auth: McpCatalogAuthKind::OptionalBearer,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.cloudflare.com/mcp",
        },
        credential_env: &["CLOUDFLARE_API_TOKEN"],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://developers.cloudflare.com/agents/model-context-protocol/mcp-servers-for-cloudflare/",
    },
    McpCatalogEntry {
        id: "stripe",
        server_name: "stripe",
        display_name: "Stripe",
        description: "Stripe MCP server for account resources and Stripe documentation.",
        category: "business",
        profiles: &["business"],
        status: McpCatalogStatus::Supported,
        risk: McpCatalogRisk::AccountReadWrite,
        auth: McpCatalogAuthKind::OptionalBearer,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.stripe.com",
        },
        credential_env: &["STRIPE_SECRET_KEY"],
        enabled_tools: &["search_stripe_documentation"],
        disabled_tools: &[],
        docs_url: "https://docs.stripe.com/mcp",
    },
    McpCatalogEntry {
        id: "figma",
        server_name: "figma",
        display_name: "Figma",
        description: "Figma MCP server for design context and Dev Mode workflows.",
        category: "design",
        profiles: &["business"],
        status: McpCatalogStatus::CatalogOnly,
        risk: McpCatalogRisk::ReadMostlyWorkspace,
        auth: McpCatalogAuthKind::OAuth,
        transport: McpCatalogTransport::StreamableHttp {
            url: "https://mcp.figma.com/mcp",
        },
        credential_env: &[],
        enabled_tools: &[],
        disabled_tools: &[],
        docs_url: "https://developers.figma.com/docs/figma-mcp-server/",
    },
];

/// Returns the built-in MCP catalog.
pub fn builtin_catalog_entries() -> Vec<McpCatalogEntryView> {
    CATALOG.iter().map(entry_view).collect()
}

/// Returns the built-in MCP profiles.
pub fn builtin_catalog_profiles() -> Vec<McpCatalogProfileView> {
    PROFILES
        .iter()
        .map(|profile| McpCatalogProfileView {
            id: profile.id.to_string(),
            display_name: profile.display_name.to_string(),
            description: profile.description.to_string(),
            entry_ids: CATALOG
                .iter()
                .filter(|entry| entry.profiles.contains(&profile.id))
                .map(|entry| entry.id.to_string())
                .collect(),
        })
        .collect()
}

/// Fetches one built-in MCP catalog entry.
pub fn builtin_catalog_entry(id: &str) -> Option<McpCatalogEntryView> {
    CATALOG.iter().find(|entry| entry.id == id).map(entry_view)
}

/// Fetches one built-in MCP profile.
pub fn builtin_catalog_profile(id: &str) -> Option<McpCatalogProfileView> {
    builtin_catalog_profiles()
        .into_iter()
        .find(|profile| profile.id == id)
}

pub(crate) fn expand_catalog_profiles_with_secrets(
    profiles: &[String],
    credentials: &McpResolvedSecrets,
) -> Result<Vec<(McpServerConfig, Vec<String>, String)>> {
    let profiles = normalize_profiles(profiles)?;
    let mut selected_entries =
        BTreeMap::<&'static str, (&'static McpCatalogEntry, BTreeSet<String>)>::new();
    for profile in profiles {
        ensure_known_profile(&profile)?;
        for entry in CATALOG
            .iter()
            .filter(|entry| entry.profiles.contains(&profile.as_str()))
        {
            if entry.status != McpCatalogStatus::Supported {
                continue;
            }
            selected_entries
                .entry(entry.id)
                .or_insert_with(|| (entry, BTreeSet::new()))
                .1
                .insert(profile.clone());
        }
    }
    Ok(selected_entries
        .into_iter()
        .filter(|(_entry_id, (entry, _profiles))| {
            !entry.credential_env.iter().any(|env_name| {
                credentials
                    .revoked_secret_refs
                    .contains(&catalog_credential_secret_ref(entry.id, env_name))
            })
        })
        .map(|(entry_id, (entry, profiles))| {
            entry_config(entry, credentials)
                .map(|config| (config, profiles.into_iter().collect(), entry_id.to_string()))
        })
        .collect::<Result<Vec<_>>>()?)
}

fn normalize_profiles(profiles: &[String]) -> Result<Vec<String>> {
    let mut normalized = BTreeSet::new();
    for profile in profiles {
        for part in profile.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            normalized.insert(trimmed.to_ascii_lowercase());
        }
    }
    Ok(normalized.into_iter().collect())
}

fn ensure_known_profile(profile: &str) -> Result<()> {
    if PROFILES.iter().any(|candidate| candidate.id == profile) {
        return Ok(());
    }
    Err(anyhow!("unknown MCP catalog profile `{profile}`"))
}

struct ResolvedCatalogCredential {
    env_name: &'static str,
    secret_ref: Option<String>,
    value: String,
}

fn entry_config(
    entry: &McpCatalogEntry,
    credentials: &McpResolvedSecrets,
) -> Result<McpServerConfig> {
    let resolved_credentials = resolve_catalog_credentials(entry, credentials);
    if entry.auth == McpCatalogAuthKind::Bearer
        && !entry.credential_env.is_empty()
        && resolved_credentials.is_empty()
    {
        return Err(anyhow!(
            "MCP catalog entry `{}` requires credential {}; store it with `mcp auth set` before startup",
            entry.id,
            entry.credential_env.join(", ")
        ));
    }
    let credential_secret_refs = resolved_credentials
        .iter()
        .filter_map(|credential| credential.secret_ref.clone())
        .collect();
    Ok(McpServerConfig {
        name: entry.server_name.to_string(),
        startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
        tool_timeout_ms: DEFAULT_TOOL_TIMEOUT_MS,
        required: false,
        enabled_tools: entry
            .enabled_tools
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        disabled_tools: entry
            .disabled_tools
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        inherit_env: false,
        credential_secret_refs,
        transport: match entry.transport {
            McpCatalogTransport::StreamableHttp { url } => {
                let bearer_token = resolved_credentials
                    .first()
                    .map(|credential| credential.value.clone());
                McpServerTransport::StreamableHttp {
                    url: url.to_string(),
                    headers: BTreeMap::new(),
                    auth: bearer_token
                        .map(|token| McpHttpAuth::BearerToken { token })
                        .unwrap_or_default(),
                }
            }
            McpCatalogTransport::Stdio { command, args, env } => {
                let mut server_env = env
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                    .collect::<BTreeMap<_, _>>();
                for credential in &resolved_credentials {
                    server_env.insert(credential.env_name.to_string(), credential.value.clone());
                }
                McpServerTransport::Stdio {
                    command: command.to_string(),
                    args: args.iter().map(|arg| (*arg).to_string()).collect(),
                    env: server_env,
                    cwd: None,
                }
            }
        },
    })
}

fn resolve_catalog_credentials(
    entry: &McpCatalogEntry,
    credentials: &McpResolvedSecrets,
) -> Vec<ResolvedCatalogCredential> {
    entry
        .credential_env
        .iter()
        .filter_map(|env_name| {
            let secret_ref = catalog_credential_secret_ref(entry.id, env_name);
            if credentials.revoked_secret_refs.contains(&secret_ref) {
                return None;
            }
            if let Some(value) = credentials.secret_values.get(&secret_ref) {
                return Some(ResolvedCatalogCredential {
                    env_name,
                    secret_ref: Some(secret_ref),
                    value: value.clone(),
                });
            }
            if credentials.allow_env_fallback {
                return env::var_os(env_name).map(|value| ResolvedCatalogCredential {
                    env_name,
                    secret_ref: None,
                    value: value.to_string_lossy().into_owned(),
                });
            }
            None
        })
        .collect()
}

fn entry_view(entry: &McpCatalogEntry) -> McpCatalogEntryView {
    let (transport, endpoint) = match entry.transport {
        McpCatalogTransport::StreamableHttp { url } => {
            ("streamable_http".to_string(), url.to_string())
        }
        McpCatalogTransport::Stdio { command, args, .. } => {
            ("stdio".to_string(), format!("{command} {}", args.join(" ")))
        }
    };
    McpCatalogEntryView {
        id: entry.id.to_string(),
        server_name: entry.server_name.to_string(),
        display_name: entry.display_name.to_string(),
        description: entry.description.to_string(),
        category: entry.category.to_string(),
        profiles: entry
            .profiles
            .iter()
            .map(|profile| (*profile).to_string())
            .collect(),
        status: entry.status,
        risk: entry.risk,
        auth: entry.auth,
        transport,
        endpoint,
        credential_env: entry
            .credential_env
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        credential_secret_refs: entry
            .credential_env
            .iter()
            .map(|value| catalog_credential_secret_ref(entry.id, value))
            .collect(),
        enabled_tools: entry
            .enabled_tools
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        disabled_tools: entry
            .disabled_tools
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        docs_url: entry.docs_url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        McpCatalogStatus, McpResolvedSecrets, builtin_catalog_entries,
        catalog_credential_secret_ref, expand_catalog_profiles_with_secrets,
    };
    use crate::config::{McpHttpAuth, McpServerTransport};

    #[test]
    fn catalog_entries_have_unique_ids_and_server_names() {
        let entries = builtin_catalog_entries();
        let ids = entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<BTreeSet<_>>();
        let server_names = entries
            .iter()
            .map(|entry| entry.server_name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), entries.len(), "catalog ids must be unique");
        assert_eq!(
            server_names.len(),
            entries.len(),
            "catalog server names must be unique"
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.status == McpCatalogStatus::CatalogOnly),
            "catalog should be explicit about entries Kheish cannot directly start yet"
        );
        assert!(
            entries
                .iter()
                .find(|entry| entry.id == "linear")
                .expect("linear entry should exist")
                .credential_secret_refs
                .contains(&"mcp.linear.LINEAR_API_KEY".to_string())
        );
    }

    #[test]
    fn docs_profile_expands_only_supported_docs_servers() {
        let configs =
            expand_catalog_profiles_with_secrets(&["docs".to_string()], &Default::default())
                .expect("profile");
        assert!(configs.iter().any(|(config, profiles, entry)| {
            config.name == "openaiDeveloperDocs"
                && profiles == &vec!["docs".to_string()]
                && entry == "openai-docs"
        }));
        assert!(configs.iter().all(|(config, _, _)| matches!(
            config.transport,
            McpServerTransport::StreamableHttp { .. }
        )));
    }

    #[test]
    fn unknown_profile_fails_closed() {
        let error =
            expand_catalog_profiles_with_secrets(&["missing".to_string()], &Default::default())
                .expect_err("unknown profile should fail");
        assert!(error.to_string().contains("unknown MCP catalog profile"));
    }

    #[test]
    fn catalog_profile_credentials_resolve_from_secret_refs() {
        let configs = expand_catalog_profiles_with_secrets(
            &["planning".to_string()],
            &McpResolvedSecrets {
                secret_values: BTreeMap::from([(
                    "mcp.linear.LINEAR_API_KEY".to_string(),
                    "linear-secret".to_string(),
                )]),
                revoked_secret_refs: Default::default(),
                allow_env_fallback: false,
            },
        )
        .expect("planning profile should expand");
        let linear = configs
            .iter()
            .find(|(config, _, entry_id)| config.name == "linear" && entry_id == "linear")
            .expect("linear config should be present");
        assert_eq!(
            linear.0.credential_secret_refs,
            vec!["mcp.linear.LINEAR_API_KEY".to_string()]
        );
        match &linear.0.transport {
            McpServerTransport::StreamableHttp { auth, .. } => assert_eq!(
                auth,
                &McpHttpAuth::BearerToken {
                    token: "linear-secret".to_string()
                }
            ),
            other => panic!("unexpected transport: {other:?}"),
        }
    }

    #[test]
    fn catalog_profile_skips_revoked_secret_refs() {
        let configs = expand_catalog_profiles_with_secrets(
            &["planning".to_string()],
            &McpResolvedSecrets {
                secret_values: BTreeMap::new(),
                revoked_secret_refs: BTreeSet::from([catalog_credential_secret_ref(
                    "linear",
                    "LINEAR_API_KEY",
                )]),
                allow_env_fallback: false,
            },
        )
        .expect("revoked catalog credentials should skip affected entries");
        assert!(
            configs
                .iter()
                .all(|(config, _, entry_id)| config.name != "linear" && entry_id != "linear")
        );
    }

    #[test]
    fn catalog_optional_credentials_do_not_claim_missing_secret_refs() {
        let configs =
            expand_catalog_profiles_with_secrets(&["planning".to_string()], &Default::default())
                .expect("planning profile should expand");
        let linear = configs
            .iter()
            .find(|(config, _, entry_id)| config.name == "linear" && entry_id == "linear")
            .expect("linear config should be present");
        assert!(linear.0.credential_secret_refs.is_empty());
        match &linear.0.transport {
            McpServerTransport::StreamableHttp { auth, .. } => {
                assert_eq!(auth, &McpHttpAuth::None);
            }
            other => panic!("unexpected transport: {other:?}"),
        }
    }
}
