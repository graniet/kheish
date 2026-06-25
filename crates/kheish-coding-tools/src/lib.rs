//! Default coding-oriented tools for Kheish runtimes.
//!
//! The tools in this crate are intentionally provider-neutral and daemon-safe:
//! they validate paths against a configured workspace root, keep schemas stable,
//! and expose a compact surface area that is practical for agentic coding loops.

mod bash;
mod editing;
mod files;
mod patching;
mod search;
mod shared;
#[cfg(test)]
mod tests;
mod web;

pub use bash::{
    BashCommandWorkdirGuard, configure_bash_command_workdir,
    configure_resolved_bash_command_workdir, execute_bash_foreground, resolve_bash_workdir,
};
pub use shared::CodingToolConfig;
pub use web::{
    ProviderWebSearchService, WebSearchBackendOutput, WebSearchHit, WebSearchRequest,
    WebSearchRoute,
};

use std::sync::Arc;

use kheish_runtime::ToolRuntime;

use crate::bash::BashTool;
use crate::editing::EditFileTool;
use crate::files::{ReadFileTool, WriteFileTool};
use crate::patching::ApplyPatchTool;
use crate::search::{GlobSearchTool, GrepSearchTool, ListFilesTool};
use crate::shared::SharedConfig;
use crate::web::{WebFetchTool, WebSearchTool};

/// Registers the default coding tools into a runtime.
pub fn register_default_coding_tools(
    runtime: &mut ToolRuntime,
    config: CodingToolConfig,
    provider_search: Option<Arc<dyn ProviderWebSearchService>>,
) {
    let shared = Arc::new(SharedConfig::new(config));
    runtime.register(ReadFileTool::new(shared.clone()));
    runtime.register(WriteFileTool::new(shared.clone()));
    runtime.register(EditFileTool::new(shared.clone()));
    runtime.register(ApplyPatchTool::new(shared.clone()));
    runtime.register(ListFilesTool::new(shared.clone()));
    runtime.register(GlobSearchTool::new(shared.clone()));
    runtime.register(GrepSearchTool::new(shared.clone()));
    runtime.register(BashTool::new(shared.clone()));
    runtime.register(WebSearchTool::with_provider_search(
        shared.clone(),
        provider_search,
    ));
    runtime.register(WebFetchTool::new(shared));
}
