//! Shared route-selection helpers for CLI commands.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const ROUTE_INVENTORY_METADATA_FILE: &str = "route-inventory.json";

/// Non-secret metadata about the route inventory loaded by the running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteInventoryMetadata {
    pub(crate) version: u32,
    pub(crate) source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) routes_file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) routes_file_sha256: Option<String>,
    pub(crate) loaded_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_route: Option<String>,
    #[serde(default)]
    pub(crate) route_ids: Vec<String>,
}

/// Persists route-source metadata for runtime Doctor drift diagnostics.
pub(crate) fn write_route_inventory_metadata(
    state_root: &Path,
    routes_file: Option<&Path>,
    routes: &[kheish_daemon::ConfiguredModelRoute],
) -> Result<()> {
    fs::create_dir_all(state_root).with_context(|| {
        format!(
            "failed to create state root for route inventory metadata {}",
            state_root.display()
        )
    })?;
    let routes_file_path = routes_file
        .map(|path| {
            fs::canonicalize(path)
                .with_context(|| format!("failed to canonicalize routes file {}", path.display()))
        })
        .transpose()?
        .map(|path| path.display().to_string());
    let routes_file_sha256 = routes_file
        .map(|path| {
            fs::read_to_string(path)
                .map(|raw| kheish_codec::digest_text(&raw))
                .with_context(|| format!("failed to hash routes file {}", path.display()))
        })
        .transpose()?;
    let metadata = RouteInventoryMetadata {
        version: 1,
        source_kind: if routes_file.is_some() {
            "routes_file".to_string()
        } else {
            "serve_args".to_string()
        },
        routes_file_path,
        routes_file_sha256,
        loaded_at_ms: now_ms(),
        default_route: routes.first().map(|route| route.route_id().to_string()),
        route_ids: routes
            .iter()
            .map(|route| route.route_id().to_string())
            .collect(),
    };
    let path = route_inventory_metadata_path(state_root);
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&metadata)?;
    fs::write(&tmp_path, bytes).with_context(|| {
        format!(
            "failed to write route inventory metadata {}",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to atomically install route inventory metadata {}",
            path.display()
        )
    })?;
    Ok(())
}

pub(crate) fn read_route_inventory_metadata(
    state_root: &Path,
) -> Result<Option<RouteInventoryMetadata>> {
    let path = route_inventory_metadata_path(state_root);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read route inventory metadata {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| {
            format!(
                "failed to decode route inventory metadata {}",
                path.display()
            )
        })
        .map(Some)
}

pub(crate) fn current_routes_file_sha256(
    metadata: &RouteInventoryMetadata,
) -> Result<Option<String>> {
    let Some(path) = metadata.routes_file_path.as_deref() else {
        return Ok(None);
    };
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read loaded routes file {path}"))?;
    Ok(Some(kheish_codec::digest_text(&raw)))
}

fn route_inventory_metadata_path(state_root: &Path) -> PathBuf {
    state_root.join(ROUTE_INVENTORY_METADATA_FILE)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_millis() as u64
}

/// Parsed route/model selector produced from a CLI model string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedModelSelector {
    pub(crate) provider: Option<String>,
    pub(crate) model: String,
}

/// Loads all known route ids from the runtime view exposed by the daemon.
pub(crate) async fn fetch_known_route_ids(
    client: &crate::cli::DaemonHttpClient,
) -> Result<BTreeSet<String>> {
    let runtime = client
        .get_json::<kheish_daemon::RuntimeSettingsView>("/v1/runtime")
        .await?;
    let mut route_ids = runtime
        .routes
        .into_iter()
        .map(|route| route.route_id)
        .collect::<BTreeSet<_>>();
    if let Some(default_route) = runtime.default_route {
        route_ids.insert(default_route.route_id);
    }
    if route_ids.is_empty() {
        if let Some(route_id) = runtime.route_id {
            route_ids.insert(route_id);
        }
    }
    Ok(route_ids)
}

/// Parses one `route/model` selector while preserving plain model names.
pub(crate) fn parse_model_selector(
    value: &str,
    known_route_ids: &BTreeSet<String>,
) -> Result<ParsedModelSelector> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("model selector cannot be empty");
    }
    if let Some((candidate_route, raw_model)) = trimmed.split_once('/') {
        if known_route_ids.contains(candidate_route) {
            let model = raw_model.trim();
            if model.is_empty() {
                bail!("model selector `{trimmed}` must include a model after the route id");
            }
            return Ok(ParsedModelSelector {
                provider: Some(candidate_route.to_string()),
                model: model.to_string(),
            });
        }
    }
    Ok(ParsedModelSelector {
        provider: None,
        model: trimmed.to_string(),
    })
}

/// Normalizes provider and generation overrides against the configured route inventory.
pub(crate) fn normalize_provider_and_generation(
    provider: Option<String>,
    generation: Option<kheish_runtime::ModelGenerationConfig>,
    known_route_ids: &BTreeSet<String>,
) -> Result<(
    Option<String>,
    Option<kheish_runtime::ModelGenerationConfig>,
)> {
    let mut provider = provider.filter(|value| !value.trim().is_empty());
    let mut generation = generation;
    if let Some(config) = generation.as_mut() {
        if let Some(model) = config.model.take() {
            let selector = parse_model_selector(&model, known_route_ids)?;
            merge_selected_route(&mut provider, selector.provider.as_deref(), "--model")?;
            config.model = Some(selector.model);
        }
        if let Some(fallback_model) = config.fallback_model.take() {
            let selector = parse_model_selector(&fallback_model, known_route_ids)?;
            merge_selected_route(
                &mut provider,
                selector.provider.as_deref(),
                "--fallback-model",
            )?;
            config.fallback_model = Some(selector.model);
        }
    }
    Ok((provider, generation))
}

fn merge_selected_route(
    provider: &mut Option<String>,
    selected_route: Option<&str>,
    source_flag: &str,
) -> Result<()> {
    let Some(selected_route) = selected_route else {
        return Ok(());
    };
    match provider {
        Some(current) if current != selected_route => {
            bail!(
                "{source_flag} route `{selected_route}` conflicts with selected route `{current}`"
            );
        }
        Some(_) => {}
        None => *provider = Some(selected_route.to_string()),
    }
    Ok(())
}
