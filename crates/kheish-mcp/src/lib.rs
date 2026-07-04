//! Model Context Protocol integration for Kheish.

mod catalog;
mod client;
mod config;
mod manager;
mod tools;

pub use catalog::{
    McpCatalogAuthKind, McpCatalogEntryView, McpCatalogProfileView, McpCatalogRisk,
    McpCatalogStatus, McpResolvedSecrets, builtin_catalog_entries, builtin_catalog_entry,
    builtin_catalog_profile, builtin_catalog_profiles, catalog_credential_secret_ref,
    normalize_catalog_profile_name,
};
pub use config::{
    CodexCompatOptions, CodexServerConfig, LoadedMcpServerConfig, McpLoadOptions, McpLoadResult,
    McpServerConfig, McpServerSource, McpServerTransport, codex_server_to_config,
    default_codex_credentials_path, default_codex_mcp_config_path, load_codex_mcp_servers,
    load_mcp_servers,
};
pub use manager::{McpManager, McpRuntimeSnapshot, McpServerInstruction, McpServerSnapshot};
