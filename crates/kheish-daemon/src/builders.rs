//! Daemon service builders and provider bootstrap helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, RwLock as StdRwLock};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use kheish_agent::{AgentOrchestrator, AgentSupervisor};
use kheish_auth::{AuthManager, AuthSlotId};
use kheish_core::{HookDispatcher, LoopPolicy, ModelDriver};
use kheish_mcp::{
    CodexCompatOptions, McpLoadOptions, McpManager, McpResolvedSecrets, McpRuntimeSnapshot,
};
use kheish_output::OutputHost;
use kheish_runtime::{
    AgentRuntimeDependencies, DebugCaptureLevel, DebugControl, ModelBudget, ModelRetryPolicy,
    PermissionBehavior, PermissionEngine, PermissionRule, PermissionScope, RuntimeObserver,
    SystemPromptBuilder, SystemPromptEnvironment, SystemPromptSettings, ToolRuntime,
};
use kheish_session::FileSessionStore;

use crate::audio_generation::{
    AudioGenerationBackend, AudioGenerationService, audio_generation_backend_from_route,
};
use crate::connectors::{build_delivery_dispatcher, register_output_plugins};
use crate::control_tools::{DaemonToolControlHandle, register_daemon_control_tools};
use crate::delivery::DeliveryQueue;
use crate::events::{DaemonEventBus, DaemonObserver};
use crate::hooks::DaemonHookDispatcher;
use crate::image_generation::{
    AdditionalImageBackendConfig, GoogleImageGenerationBackend, ImageGenerationBackend,
    ImageGenerationService, OpenAiImageGenerationBackend, OpenRouterImageGenerationBackend,
    XAiImageGenerationBackend,
};
use crate::model_routing::{
    ConfiguredModelRoute, DaemonModelControl, ModelRouteConfig, RoutedModelControl,
};
use crate::service::DaemonService;
use crate::services::ConnectorService;
use crate::transcription::{
    AdditionalTranscriptionBackendConfig, AudioTranscriptionBackend, TranscriptionService,
    transcription_backend_from_additional_config, transcription_backend_from_route,
};
use crate::web_search::build_web_search_service;
use crate::{
    DaemonConfig, DaemonOutputPlugin, DaemonOutputReceiver, DaemonOutputRecord,
    DaemonOutputSourceKind, DaemonState, FileDaemonStore, FileDebugStore,
};
use kheish_coding_tools::{CodingToolConfig, register_default_coding_tools};

pub(crate) fn daemon_model_retry_policy() -> ModelRetryPolicy {
    ModelRetryPolicy {
        max_attempts: 2,
        base_backoff_ms: 1_000,
        stream_timeout_ms: 240_000,
        inactivity_timeout_ms: 90_000,
    }
}

#[cfg(test)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn daemon_model_budget() -> ModelBudget {
    ModelBudget {
        max_total_output_tokens: 1_000_000,
        max_total_cost_usd: 500.0,
    }
}

pub(crate) fn daemon_model_budget_for_config(config: &DaemonConfig) -> ModelBudget {
    ModelBudget {
        max_total_output_tokens: config.model_budget_max_total_output_tokens,
        max_total_cost_usd: config.model_budget_max_total_cost_usd,
    }
}

fn default_session_permission_rules() -> Vec<PermissionRule> {
    vec![
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "bash".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("shell command requires approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "write_file".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("file write requires approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "edit_file".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("file edit requires approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "apply_patch".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("file patch requires approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "mcp__*".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some(
                "MCP tool calls can affect external systems and require approval".to_string(),
            ),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "list_mcp_resources".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("MCP resource listing requires approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "list_mcp_resource_templates".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("MCP resource template listing requires approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "read_mcp_resource".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("MCP resource reads require approval".to_string()),
        },
        PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "exit_plan_mode".to_string(),
            behavior: PermissionBehavior::Ask,
            reason: Some("leaving plan mode requires approval".to_string()),
        },
    ]
}

fn allow_all_session_permission_rule() -> PermissionRule {
    PermissionRule {
        scope: PermissionScope::Session,
        tool_name_pattern: "*".to_string(),
        behavior: PermissionBehavior::Allow,
        reason: None,
    }
}

pub(crate) fn spawn_output_collector<M>(
    state: Arc<DaemonState<M>>,
    mut receiver: DaemonOutputReceiver,
) where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    tokio::spawn(async move {
        while let Some(delivery) = receiver.recv().await {
            let envelope = delivery.response;
            let run_id = envelope
                .metadata
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let record = DaemonOutputRecord {
                session_id: envelope.conversation.session_id.clone(),
                run_id: run_id.clone(),
                content: envelope.content,
                parts: envelope.parts,
                artifacts: envelope.artifacts,
                source_kind: envelope
                    .metadata
                    .get("output_kind")
                    .and_then(Value::as_str)
                    .and_then(DaemonOutputSourceKind::from_metadata_str),
                plugin: envelope.reply.as_ref().map(|reply| reply.plugin.clone()),
                address: envelope.reply.map(|reply| reply.address),
            };
            let _ = delivery
                .ack
                .send(state.record_output(record, run_id.as_deref()).await);
        }
    });
}

pub(crate) fn restore_supervisor(
    store: &FileDaemonStore,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Arc<AgentSupervisor>> {
    let supervisor = Arc::new(match store.load_supervisor()? {
        Some(snapshot) => AgentSupervisor::try_restore(snapshot, observer)?,
        None => AgentSupervisor::new(observer),
    });
    supervisor.set_audit_sink(Arc::new(store.clone()));
    Ok(supervisor)
}

pub(crate) fn next_session_seed(store: &FileDaemonStore) -> u64 {
    store
        .load_index()
        .ok()
        .map(|index| {
            index
                .sessions
                .keys()
                .filter_map(|session_id: &String| session_id.strip_prefix("session-"))
                .filter_map(|suffix: &str| suffix.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn route_with_asset_root(route: ModelRouteConfig, state_root: &Path) -> ModelRouteConfig {
    match route {
        ModelRouteConfig::Anthropic(mut config) => {
            config.asset_root = Some(state_root.join("assets"));
            ModelRouteConfig::Anthropic(config)
        }
        ModelRouteConfig::Google(mut config) => {
            config.asset_root = Some(state_root.join("assets"));
            ModelRouteConfig::Google(config)
        }
        ModelRouteConfig::OpenAi(mut config) => {
            config.asset_root = Some(state_root.join("assets"));
            ModelRouteConfig::OpenAi(config)
        }
        ModelRouteConfig::OpenRouter(mut config) => {
            config.asset_root = Some(state_root.join("assets"));
            ModelRouteConfig::OpenRouter(config)
        }
        ModelRouteConfig::XAi(mut config) => {
            config.asset_root = Some(state_root.join("assets"));
            ModelRouteConfig::XAi(config)
        }
    }
}

fn configured_route_with_asset_root(
    route: ConfiguredModelRoute,
    state_root: &Path,
) -> ConfiguredModelRoute {
    let route_config = route_with_asset_root(route.route_config().clone(), state_root);
    route.with_route_config(route_config)
}

fn build_image_generation_service(
    routes: &[ConfiguredModelRoute],
    extra_backends: &[AdditionalImageBackendConfig],
    assets: Arc<crate::assets::FileAssetStore>,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Option<Arc<ImageGenerationService>>> {
    let mut backends = std::collections::BTreeMap::new();
    let mut default_route_id = None;
    for route in routes {
        if !route.capabilities().image_generation {
            continue;
        }
        if let Some((route_id, backend)) = image_generation_backend_from_route(
            route.route_id(),
            route.route_config(),
            observer.clone(),
        )? {
            if default_route_id.is_none() {
                default_route_id = Some(route_id.clone());
            }
            backends.entry(route_id).or_insert(backend);
        }
    }
    for backend_config in extra_backends {
        let (route_id, backend) =
            image_generation_backend_from_additional_config(backend_config, observer.clone())?;
        if default_route_id.is_none() {
            default_route_id = Some(route_id.clone());
        }
        backends.insert(route_id, backend);
    }
    if backends.is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::new(ImageGenerationService::new(
        backends,
        default_route_id.expect("default route is set when at least one backend exists"),
        assets,
    )?)))
}

fn build_transcription_service(
    routes: &[ConfiguredModelRoute],
    extra_backends: &[AdditionalTranscriptionBackendConfig],
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Option<Arc<TranscriptionService>>> {
    let mut backends = BTreeMap::<String, Arc<dyn AudioTranscriptionBackend>>::new();
    let mut default_route_id = None;
    for route in routes {
        if !route.capabilities().transcription {
            continue;
        }
        if let Some((route_id, backend)) = transcription_backend_from_route(
            route.route_id(),
            route.route_config(),
            observer.clone(),
        )? {
            if default_route_id.is_none() {
                default_route_id = Some(route_id.clone());
            }
            backends.insert(route_id, backend);
        }
    }
    for backend_config in extra_backends {
        let (route_id, backend) =
            transcription_backend_from_additional_config(backend_config, observer.clone())?;
        if default_route_id.is_none() {
            default_route_id = Some(route_id.clone());
        }
        backends.insert(route_id, backend);
    }
    if backends.is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::new(TranscriptionService::new(
        backends,
        default_route_id.expect("default transcription route is set when one backend exists"),
    )?)))
}

fn build_audio_generation_service(
    routes: &[ConfiguredModelRoute],
    assets: Arc<crate::assets::FileAssetStore>,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Option<Arc<AudioGenerationService>>> {
    let mut backends = BTreeMap::<String, Arc<dyn AudioGenerationBackend>>::new();
    let mut default_route_id = None;
    for route in routes {
        if !route.capabilities().audio_generation {
            continue;
        }
        if let Some((route_id, backend)) = audio_generation_backend_from_route(
            route.route_id(),
            route.route_config(),
            observer.clone(),
        )? {
            if default_route_id.is_none() {
                default_route_id = Some(route_id.clone());
            }
            backends.insert(route_id, backend);
        }
    }
    if backends.is_empty() {
        return Ok(None);
    }
    Ok(Some(Arc::new(AudioGenerationService::new(
        backends,
        default_route_id.expect("default audio route is set when at least one backend exists"),
        assets,
    )?)))
}

fn image_generation_backend_from_additional_config(
    config: &AdditionalImageBackendConfig,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<(String, Arc<dyn ImageGenerationBackend>)> {
    image_generation_backend_from_route(config.route_id(), config.route_config(), observer)?
        .context("additional image backend route did not support image generation")
}

fn image_generation_backend_from_route(
    route_id: &str,
    route: &ModelRouteConfig,
    observer: Arc<dyn RuntimeObserver>,
) -> Result<Option<(String, Arc<dyn ImageGenerationBackend>)>> {
    if !route.supports_image_generation_backend() {
        return Ok(None);
    }
    match route {
        ModelRouteConfig::Google(config) => {
            let backend = GoogleImageGenerationBackend::from_config(config.clone(), observer)?;
            Ok(Some((route_id.to_string(), Arc::new(backend))))
        }
        ModelRouteConfig::OpenAi(config) => {
            let backend = OpenAiImageGenerationBackend::from_config(config.clone(), observer)?;
            Ok(Some((route_id.to_string(), Arc::new(backend))))
        }
        ModelRouteConfig::OpenRouter(config) => {
            let backend = OpenRouterImageGenerationBackend::from_config(config.clone(), observer)?;
            Ok(Some((route_id.to_string(), Arc::new(backend))))
        }
        ModelRouteConfig::XAi(config) => {
            let backend = XAiImageGenerationBackend::from_config(config.clone(), observer)?;
            Ok(Some((route_id.to_string(), Arc::new(backend))))
        }
        ModelRouteConfig::Anthropic(_) => Ok(None),
    }
}

fn has_image_generation_backend(
    routes: &[ConfiguredModelRoute],
    extra_backends: &[AdditionalImageBackendConfig],
) -> bool {
    routes
        .iter()
        .any(|route| route.capabilities().image_generation)
        || !extra_backends.is_empty()
}

fn has_image_edit_backend(
    routes: &[ConfiguredModelRoute],
    extra_backends: &[AdditionalImageBackendConfig],
) -> bool {
    routes.iter().any(|route| route.capabilities().image_edit)
        || extra_backends
            .iter()
            .any(|backend| backend.route_config().supports_image_edit_backend())
}

fn has_audio_generation_backend(routes: &[ConfiguredModelRoute]) -> bool {
    routes
        .iter()
        .any(|route| route.capabilities().audio_generation)
}

fn route_has_image_generation_backend(
    route: &ConfiguredModelRoute,
    extra_backends: &[AdditionalImageBackendConfig],
) -> bool {
    route.route_config().supports_image_generation_backend()
        || extra_backends.iter().any(|backend| {
            backend.route_id() == route.route_id()
                && backend.route_config().supports_image_generation_backend()
        })
}

fn route_has_image_edit_backend(
    route: &ConfiguredModelRoute,
    extra_backends: &[AdditionalImageBackendConfig],
) -> bool {
    route.route_config().supports_image_edit_backend()
        || extra_backends.iter().any(|backend| {
            backend.route_id() == route.route_id()
                && backend.route_config().supports_image_edit_backend()
        })
}

fn route_has_audio_generation_backend(route: &ConfiguredModelRoute) -> bool {
    route.route_config().supports_audio_generation_backend()
}

fn route_has_transcription_backend(
    route: &ConfiguredModelRoute,
    extra_backends: &[AdditionalTranscriptionBackendConfig],
) -> bool {
    route.route_config().supports_transcription_backend()
        || extra_backends.iter().any(|backend| {
            backend.route_id() == route.route_id()
                && backend.route_config().supports_transcription_backend()
        })
}

fn validate_route_capability_overrides(
    routes: &[ConfiguredModelRoute],
    extra_backends: &[AdditionalImageBackendConfig],
    extra_transcription_backends: &[AdditionalTranscriptionBackendConfig],
) -> Result<()> {
    for route in routes {
        let configured = route.capabilities();
        let supported = route.route_config().supported_capabilities();
        if configured.native_web_search && !supported.native_web_search {
            anyhow::bail!(
                "route `{}` advertises native_web_search but driver `{}` does not support it",
                route.route_id(),
                route.route_config().provider_name()
            );
        }
        if configured.image_generation && !route_has_image_generation_backend(route, extra_backends)
        {
            anyhow::bail!(
                "route `{}` advertises image_generation but no matching image backend is available",
                route.route_id()
            );
        }
        if configured.image_edit && !route_has_image_edit_backend(route, extra_backends) {
            anyhow::bail!(
                "route `{}` advertises image_edit but no matching image edit backend is available",
                route.route_id()
            );
        }
        if configured.audio_generation && !route_has_audio_generation_backend(route) {
            anyhow::bail!(
                "route `{}` advertises audio_generation but no matching audio generation backend is available",
                route.route_id()
            );
        }
        if configured.transcription
            && !route_has_transcription_backend(route, extra_transcription_backends)
        {
            anyhow::bail!(
                "route `{}` advertises transcription but no matching transcription backend is available",
                route.route_id()
            );
        }
    }
    Ok(())
}

/// Builds a production-ready daemon service around the provided primary and fallback routes.
pub async fn build_provider_daemon<R>(
    config: DaemonConfig,
    route_configs: Vec<R>,
    extra_image_backends: Vec<AdditionalImageBackendConfig>,
    extra_transcription_backends: Vec<AdditionalTranscriptionBackendConfig>,
    auth_manager: Arc<AuthManager>,
) -> Result<(DaemonService, TcpListener)>
where
    R: Into<ConfiguredModelRoute>,
{
    build_provider_daemon_with_extension(
        config,
        route_configs,
        extra_image_backends,
        extra_transcription_backends,
        auth_manager,
        |_, _| Ok(()),
    )
    .await
}

/// Builds a production-ready daemon service and lets callers extend the tool runtime
/// and permission rules before MCP registration and runtime assembly.
pub async fn build_provider_daemon_with_extension<R, F>(
    config: DaemonConfig,
    route_configs: Vec<R>,
    extra_image_backends: Vec<AdditionalImageBackendConfig>,
    extra_transcription_backends: Vec<AdditionalTranscriptionBackendConfig>,
    auth_manager: Arc<AuthManager>,
    extend: F,
) -> Result<(DaemonService, TcpListener)>
where
    R: Into<ConfiguredModelRoute>,
    F: FnOnce(&mut ToolRuntime, &mut Vec<PermissionRule>) -> Result<()>,
{
    config.validate_control_plane_boundary()?;
    tokio::fs::create_dir_all(&config.workspace_root)
        .await
        .with_context(|| {
            format!(
                "failed to create daemon workspace root {}",
                config.workspace_root.display()
            )
        })?;
    let route_configs = route_configs
        .into_iter()
        .map(Into::into)
        .map(|route| configured_route_with_asset_root(route, &config.state_root))
        .collect::<Vec<_>>();
    validate_route_capability_overrides(
        &route_configs,
        &extra_image_backends,
        &extra_transcription_backends,
    )?;
    let primary_route = route_configs
        .first()
        .cloned()
        .context("daemon requires at least one provider route")?;
    let events = DaemonEventBus::new_with_persistent_epoch(
        config.event_history_capacity,
        &config.state_root.join("events").join("event-id-epoch"),
    )?;
    let debug = DebugControl::new(DebugCaptureLevel::Off);
    let debug_store = FileDebugStore::new(&config.state_root);
    let observer: Arc<dyn RuntimeObserver> =
        DaemonObserver::shared(events.clone(), debug.clone(), debug_store);
    let coding_tool_config = CodingToolConfig::new(config.workspace_root.clone());
    let tool_control = DaemonToolControlHandle::new();
    let supports_audio_generation = has_audio_generation_backend(&route_configs);
    let supports_image_generation =
        has_image_generation_backend(&route_configs, &extra_image_backends);
    let supports_image_edit = has_image_edit_backend(&route_configs, &extra_image_backends);
    let web_search_service = build_web_search_service(&route_configs, observer.clone())?;
    let mut tools = ToolRuntime::new(observer.clone());
    register_default_coding_tools(&mut tools, coding_tool_config.clone(), web_search_service);
    register_daemon_control_tools(
        &mut tools,
        tool_control.clone(),
        coding_tool_config.clone(),
        supports_audio_generation,
        supports_image_generation,
        supports_image_edit,
    );
    let mut permission_rules = default_session_permission_rules();
    extend(&mut tools, &mut permission_rules)?;
    permission_rules.push(allow_all_session_permission_rule());
    let mcp_manager = if config.mcp_config_path.is_some() || !config.mcp_catalog_profiles.is_empty()
    {
        let mcp_resolved_secrets =
            mcp_resolved_secrets_from_auth_store(auth_manager.as_ref()).await?;
        McpManager::from_load_options(
            config.workspace_root.clone(),
            McpLoadOptions {
                codex: CodexCompatOptions {
                    config_path: config.mcp_config_path.clone(),
                    credentials_path: config.mcp_credentials_path.clone(),
                    resolved_secrets: mcp_resolved_secrets.clone(),
                },
                catalog_profiles: config.mcp_catalog_profiles.clone(),
                resolved_secrets: mcp_resolved_secrets,
            },
            Some(auth_manager.clone()),
            observer.clone(),
        )
        .await?
    } else {
        None
    };
    let (
        mcp_snapshot,
        active_mcp_tools,
        connected_mcp_servers,
        credentialed_mcp_servers,
        mcp_tool_servers,
        mcp_server_instructions,
    ) = if let Some(manager) = mcp_manager.as_ref() {
        manager.register_into(&mut tools)?;
        let snapshot = manager.runtime_snapshot().await;
        let surface = snapshot.runtime_surface();
        (
            snapshot.clone(),
            surface.active_tools.clone(),
            surface.connected_servers.clone(),
            surface.credentialed_servers.clone(),
            surface.tool_servers.clone(),
            surface.server_instructions.clone(),
        )
    } else {
        (
            McpRuntimeSnapshot::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            Vec::new(),
        )
    };
    let mcp_surface = mcp_manager
        .as_ref()
        .map(|manager| manager.runtime_surface_handle())
        .unwrap_or_else(|| Arc::new(StdRwLock::new(mcp_snapshot.runtime_surface())));
    let hook_tools = Arc::new(tools.clone_without_hook_dispatcher());
    let system_prompt = Arc::new(SystemPromptBuilder::new(
        SystemPromptEnvironment::new(
            coding_tool_config.workspace_root.clone(),
            coding_tool_config.shell.clone(),
        ),
        SystemPromptSettings::default(),
    ));
    let permissions = Arc::new(PermissionEngine::new(
        vec![],
        vec![],
        permission_rules,
        observer.clone(),
    ));
    let sessions = Arc::new(FileSessionStore::new(config.state_root.join("sessions")));
    let (sender, receiver) = mpsc::unbounded_channel();
    let asset_store = Arc::new(crate::assets::FileAssetStore::new(&config.state_root)?);
    let connector_service = Arc::new(ConnectorService::load(
        &config.state_root,
        config.connectors_config_path.as_deref(),
        auth_manager.clone(),
    )?);
    let connectors = connector_service.registry();
    let external_connector_runtime =
        Arc::new(crate::connectors::ExternalConnectorRuntimeService::new());
    let delivery_queue = Arc::new(DeliveryQueue::load(
        config.state_root.join("daemon-deliveries.json"),
        build_delivery_dispatcher(
            connectors.clone(),
            external_connector_runtime.clone(),
            asset_store.clone(),
            observer.clone(),
            config.state_root.clone(),
        ),
    )?);
    let mut outputs = OutputHost::new();
    outputs.register(DaemonOutputPlugin::new(sender));
    register_output_plugins(&mut outputs, delivery_queue.clone());
    let outputs = Arc::new(outputs);
    let image_generation = build_image_generation_service(
        &route_configs,
        &extra_image_backends,
        asset_store.clone(),
        observer.clone(),
    )?;
    let audio_generation =
        build_audio_generation_service(&route_configs, asset_store.clone(), observer.clone())?;
    let transcription_service = build_transcription_service(
        &route_configs,
        &extra_transcription_backends,
        observer.clone(),
    )?;
    let retry = daemon_model_retry_policy();
    let budget = daemon_model_budget_for_config(&config);
    let primary_provider_name = primary_route.route_id().to_string();
    let (model, model_control) = RoutedModelControl::new(
        route_configs,
        retry,
        budget,
        observer.clone(),
        debug.clone(),
    )?;
    let model = Arc::new(model);
    let model_driver: Arc<dyn ModelDriver> = model.clone();
    let model_control_trait: Arc<dyn DaemonModelControl> = model_control.clone();
    let hooks = Arc::new(DaemonHookDispatcher::new(
        &config.state_root,
        model_driver,
        Some(model_control_trait.clone()),
        Some(primary_provider_name),
        system_prompt.clone(),
        hook_tools,
        observer.clone(),
        config.workspace_root.clone(),
    )?);
    let runtime_config_service = crate::services::RuntimeConfigService::new(&config.state_root)?;
    let hook_dispatcher: Arc<dyn HookDispatcher> = hooks.clone();
    tools.set_hook_dispatcher(hook_dispatcher.clone());
    let tools = Arc::new(tools);
    let runtime_tools = tools.clone();
    let daemon_skill_root = config.state_root.join("skills");
    std::fs::create_dir_all(&daemon_skill_root).with_context(|| {
        format!(
            "failed to create daemon skill root {}",
            daemon_skill_root.display()
        )
    })?;
    let mut skill_roots = vec![daemon_skill_root];
    skill_roots.extend(config.skill_roots.iter().cloned());
    let skills = Arc::new(kheish_skills::SharedSkillRegistry::discover(
        &config.workspace_root,
        &skill_roots,
    ));
    let supervisor =
        restore_supervisor(&FileDaemonStore::new(&config.state_root), observer.clone())?;
    let orchestrator = AgentOrchestrator::new(
        LoopPolicy::default(),
        AgentRuntimeDependencies {
            model,
            tools,
            permissions: permissions.clone(),
            sessions: sessions.clone(),
            outputs: outputs.clone(),
            system_prompt: system_prompt.clone(),
            hooks: hook_dispatcher,
            observer: observer.clone(),
            skills: skills.clone(),
            active_plugins: Vec::new(),
            active_mcp_tools,
            connected_mcp_servers,
            credentialed_mcp_servers,
            mcp_tool_servers,
            mcp_server_instructions,
            mcp_surface: mcp_surface.clone(),
        },
        supervisor.clone(),
    );
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let control_plane_bind = listener.local_addr().with_context(|| {
        format!(
            "failed to inspect bound control-plane address for {}",
            config.bind
        )
    })?;
    let service = DaemonService::new(
        crate::service::loopback_control_plane_base_url(control_plane_bind),
        control_plane_bind,
        config.state_root,
        config.workspace_root,
        asset_store.clone(),
        orchestrator,
        supervisor,
        permissions,
        sessions,
        system_prompt,
        hooks,
        debug,
        observer,
        runtime_tools,
        mcp_snapshot,
        mcp_surface,
        mcp_manager,
        Some(model_control_trait),
        events,
        outputs,
        receiver,
        delivery_queue,
        audio_generation,
        image_generation,
        transcription_service,
        runtime_config_service,
        auth_manager,
        skills,
        tool_control,
        connectors,
        external_connector_runtime,
        connector_service,
        config.subagent_policy.clone(),
        config.scheduler_policy.clone(),
        config.scheduler_enabled,
        config.control_plane_auth.clone(),
        config.control_plane_auth_token_files.clone(),
        config.control_plane_cors.clone(),
        config.state_root_lock_held,
    )
    .await?;
    Ok((service, listener))
}

async fn mcp_resolved_secrets_from_auth_store(
    auth_manager: &AuthManager,
) -> Result<McpResolvedSecrets> {
    let mut secret_values = BTreeMap::new();
    let mut revoked_secret_refs = BTreeSet::new();
    for status in auth_manager.list_statuses().await? {
        let slot_id = status.slot_id;
        let secret_ref = slot_id.0.clone();
        if !secret_ref.starts_with("mcp.") {
            continue;
        }
        if auth_manager.is_slot_revoked(&slot_id) {
            revoked_secret_refs.insert(secret_ref);
            continue;
        }
        if status.provider != kheish_auth::AuthProvider::Generic {
            continue;
        }
        match auth_manager.secret_value(&AuthSlotId::new(secret_ref.clone())) {
            Ok(Some(value)) => {
                secret_values.insert(secret_ref, value);
            }
            Ok(None) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to resolve MCP secret `{secret_ref}`"));
            }
        }
    }
    Ok(McpResolvedSecrets {
        secret_values,
        revoked_secret_refs,
        allow_env_fallback: false,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use kheish_auth::{AUTH_STORE_MASTER_KEY_ENV, AuthManager, AuthSlotId};
    use kheish_coding_tools::{CodingToolConfig, register_default_coding_tools};
    use kheish_runtime::{
        NoopObserver, OpenAiProviderConfig, PermissionBehavior, PermissionEngine,
        PermissionExplanation, PermissionMode, PermissionRule, PermissionScope, ToolRuntime,
    };
    use kheish_types::ToolCallRecord;
    use tempfile::tempdir;

    use super::{
        allow_all_session_permission_rule, build_audio_generation_service,
        default_session_permission_rules, has_audio_generation_backend,
        mcp_resolved_secrets_from_auth_store,
    };
    use crate::assets::FileAssetStore;
    use crate::control_tools::{DaemonToolControlHandle, register_daemon_control_tools};
    use crate::model_routing::{ConfiguredModelRoute, ModelRouteConfig, RouteCapabilities};

    #[test]
    fn wildcard_permission_rule_stays_last_after_extension_rules() {
        let mut rules = default_session_permission_rules();
        rules.push(PermissionRule {
            scope: PermissionScope::Session,
            tool_name_pattern: "security.net.scan".to_string(),
            behavior: PermissionBehavior::Allow,
            reason: Some("security scan preset".to_string()),
        });
        rules.push(allow_all_session_permission_rule());

        assert_eq!(
            rules.last().map(|rule| rule.tool_name_pattern.as_str()),
            Some("*")
        );
        assert_eq!(
            rules[rules.len() - 2].tool_name_pattern,
            "security.net.scan".to_string()
        );
    }

    #[test]
    fn default_sensitive_permission_rules_precede_wildcard_allow() {
        let mut rules = default_session_permission_rules();
        rules.push(allow_all_session_permission_rule());

        let patterns = rules
            .iter()
            .map(|rule| rule.tool_name_pattern.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            patterns,
            vec![
                "bash",
                "write_file",
                "edit_file",
                "apply_patch",
                "mcp__*",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource",
                "exit_plan_mode",
                "*"
            ]
        );
        for rule in rules.iter().take(9) {
            assert_eq!(rule.behavior, PermissionBehavior::Ask);
        }
        assert_eq!(
            rules.last().map(|rule| &rule.behavior),
            Some(&PermissionBehavior::Allow)
        );
    }

    #[test]
    fn default_permission_matrix_covers_registered_tool_surface() -> Result<()> {
        let temp = tempdir()?;
        let observer = Arc::new(NoopObserver);
        let mut tools = ToolRuntime::new(observer.clone());
        register_default_coding_tools(
            &mut tools,
            CodingToolConfig::new(temp.path().join("workspace")),
            None,
        );
        register_daemon_control_tools(
            &mut tools,
            DaemonToolControlHandle::new(),
            CodingToolConfig::new(temp.path().join("workspace")),
            true,
            true,
            true,
        );
        let mut tool_names = tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        tool_names.sort();

        assert_eq!(
            tool_names,
            vec![
                "apply_patch",
                "ask_user_question",
                "bash",
                "create_channel_stimulus",
                "create_goal",
                "edit_file",
                "edit_image",
                "emit_output",
                "enter_plan_mode",
                "exit_plan_mode",
                "generate_audio",
                "generate_image",
                "get_agent",
                "get_goal",
                "glob_search",
                "grep_search",
                "list_agent_summaries",
                "list_agents",
                "list_files",
                "list_skills",
                "message_agent",
                "read_channel_thread",
                "read_file",
                "request_parent_clarification",
                "schedule_cancel",
                "schedule_create",
                "schedule_get",
                "schedule_list",
                "schedule_pause",
                "schedule_resume",
                "schedule_trigger_now",
                "set_channel_reaction",
                "spawn_agent",
                "task_create",
                "task_delete",
                "task_get",
                "task_list",
                "task_output",
                "task_stop",
                "task_update",
                "todo_write",
                "update_goal",
                "use_skill",
                "wait_agent",
                "wake_after",
                "wake_at",
                "web_fetch",
                "web_search",
                "write_file",
            ],
        );

        let mut matrix_tool_names = tool_names;
        matrix_tool_names.extend([
            "list_mcp_resources".to_string(),
            "list_mcp_resource_templates".to_string(),
            "read_mcp_resource".to_string(),
            "mcp__linear__create_issue".to_string(),
        ]);
        matrix_tool_names.sort();

        let mut rules = default_session_permission_rules();
        rules.push(allow_all_session_permission_rule());
        let engine = PermissionEngine::new(vec![], vec![], rules, observer);
        for tool_name in matrix_tool_names {
            let default = explain_tool(&engine, PermissionMode::Default, &tool_name);
            assert_eq!(
                default.decision,
                expected_default_decision(&tool_name),
                "default decision drift for {tool_name}"
            );
            assert_eq!(
                default.base_decision.as_deref(),
                Some(expected_default_decision(&tool_name)),
                "default base decision drift for {tool_name}"
            );
            assert_eq!(
                default.mode_effect.as_deref(),
                None,
                "default mode unexpectedly transformed {tool_name}"
            );
            assert_eq!(
                matched_rule_pattern(&default),
                Some(expected_matched_rule_pattern(&tool_name)),
                "matched rule drift for {tool_name}"
            );

            let accept_edits = explain_tool(&engine, PermissionMode::AcceptEdits, &tool_name);
            assert_eq!(
                accept_edits.decision,
                expected_accept_edits_decision(&tool_name),
                "acceptEdits decision drift for {tool_name}"
            );
            assert_eq!(
                accept_edits.base_decision.as_deref(),
                Some(expected_default_decision(&tool_name)),
                "acceptEdits base decision drift for {tool_name}"
            );
            assert_eq!(
                accept_edits.mode_effect.as_deref(),
                expected_accept_edits_mode_effect(&tool_name),
                "acceptEdits mode effect drift for {tool_name}"
            );

            let bypass = explain_tool(&engine, PermissionMode::BypassPermissions, &tool_name);
            assert_eq!(
                bypass.decision,
                expected_bypass_permissions_decision(&tool_name),
                "bypassPermissions decision drift for {tool_name}"
            );
            assert_eq!(
                bypass.base_decision.as_deref(),
                Some(expected_default_decision(&tool_name)),
                "bypassPermissions base decision drift for {tool_name}"
            );
            assert_eq!(
                bypass.mode_effect.as_deref(),
                expected_bypass_permissions_mode_effect(&tool_name),
                "bypassPermissions mode effect drift for {tool_name}"
            );

            let plan = explain_tool(&engine, PermissionMode::Plan, &tool_name);
            assert_eq!(
                plan.decision,
                expected_plan_decision(&tool_name),
                "plan decision drift for {tool_name}"
            );
            assert_eq!(
                plan.base_decision.as_deref(),
                Some(expected_default_decision(&tool_name)),
                "plan base decision drift for {tool_name}"
            );
            assert_eq!(
                plan.mode_effect.as_deref(),
                expected_plan_mode_effect(&tool_name),
                "plan mode effect drift for {tool_name}"
            );

            let dont_ask = explain_tool(&engine, PermissionMode::DontAsk, &tool_name);
            assert_eq!(
                dont_ask.decision,
                expected_dont_ask_decision(&tool_name),
                "dontAsk decision drift for {tool_name}"
            );
            assert_eq!(
                dont_ask.base_decision.as_deref(),
                Some(expected_default_decision(&tool_name)),
                "dontAsk base decision drift for {tool_name}"
            );
            assert_eq!(
                dont_ask.mode_effect.as_deref(),
                expected_dont_ask_mode_effect(&tool_name),
                "dontAsk mode effect drift for {tool_name}"
            );
        }
        Ok(())
    }

    fn explain_tool(
        engine: &PermissionEngine,
        mode: PermissionMode,
        tool_name: &str,
    ) -> PermissionExplanation {
        engine.set_mode(mode);
        engine.explain(&ToolCallRecord {
            id: format!("matrix-{tool_name}"),
            name: tool_name.to_string(),
            input: serde_json::json!({}),
            assistant_message_id: None,
            assistant_provider_response_id: None,
        })
    }

    fn matched_rule_pattern(explanation: &PermissionExplanation) -> Option<&str> {
        explanation
            .matched_rule
            .as_ref()
            .map(|rule| rule.tool_name_pattern.as_str())
    }

    fn expected_matched_rule_pattern(tool_name: &str) -> &str {
        if matches!(
            tool_name,
            "bash"
                | "write_file"
                | "edit_file"
                | "apply_patch"
                | "exit_plan_mode"
                | "list_mcp_resources"
                | "list_mcp_resource_templates"
                | "read_mcp_resource"
        ) {
            tool_name
        } else if tool_name.starts_with("mcp__") {
            "mcp__*"
        } else {
            "*"
        }
    }

    fn expected_default_decision(tool_name: &str) -> &'static str {
        if is_default_ask_tool(tool_name) {
            "ask"
        } else {
            "allow"
        }
    }

    fn is_default_ask_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "bash"
                | "write_file"
                | "edit_file"
                | "apply_patch"
                | "exit_plan_mode"
                | "list_mcp_resources"
                | "list_mcp_resource_templates"
                | "read_mcp_resource"
        ) || tool_name.starts_with("mcp__")
    }

    fn expected_accept_edits_decision(tool_name: &str) -> &'static str {
        if matches!(tool_name, "write_file" | "edit_file" | "apply_patch") {
            "allow"
        } else {
            expected_default_decision(tool_name)
        }
    }

    fn expected_accept_edits_mode_effect(tool_name: &str) -> Option<&'static str> {
        if matches!(tool_name, "write_file" | "edit_file" | "apply_patch") {
            Some("accept_edits_auto_allowed_edit_approval")
        } else {
            None
        }
    }

    fn expected_bypass_permissions_decision(tool_name: &str) -> &'static str {
        if expected_default_decision(tool_name) == "ask" {
            "allow"
        } else {
            expected_default_decision(tool_name)
        }
    }

    fn expected_bypass_permissions_mode_effect(tool_name: &str) -> Option<&'static str> {
        if expected_default_decision(tool_name) == "ask" {
            Some("bypass_permissions_auto_allowed_approval")
        } else {
            None
        }
    }

    fn expected_plan_decision(tool_name: &str) -> &'static str {
        if is_default_ask_tool(tool_name) && plan_mode_default_tool_allowlist_contains(tool_name) {
            "ask"
        } else if plan_mode_default_tool_allowlist_contains(tool_name) {
            "allow"
        } else {
            "deny"
        }
    }

    fn expected_plan_mode_effect(tool_name: &str) -> Option<&'static str> {
        if is_default_ask_tool(tool_name) && plan_mode_default_tool_allowlist_contains(tool_name) {
            None
        } else if plan_mode_default_tool_allowlist_contains(tool_name) {
            Some("plan_mode_allowed_read_only_or_coordination_tool")
        } else {
            Some("plan_mode_denied_non_whitelisted_tool")
        }
    }

    fn expected_dont_ask_decision(tool_name: &str) -> &'static str {
        if expected_default_decision(tool_name) == "ask" {
            "deny"
        } else {
            "allow"
        }
    }

    fn expected_dont_ask_mode_effect(tool_name: &str) -> Option<&'static str> {
        if expected_default_decision(tool_name) == "ask" {
            Some("dont_ask_denied_approval")
        } else {
            None
        }
    }

    fn plan_mode_default_tool_allowlist_contains(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "ask_user_question"
                | "enter_plan_mode"
                | "exit_plan_mode"
                | "get_agent"
                | "get_goal"
                | "glob_search"
                | "grep_search"
                | "list_agent_summaries"
                | "list_agents"
                | "list_files"
                | "list_mcp_resource_templates"
                | "list_mcp_resources"
                | "list_skills"
                | "message_agent"
                | "read_channel_thread"
                | "read_file"
                | "read_mcp_resource"
                | "request_parent_clarification"
                | "schedule_get"
                | "schedule_list"
                | "spawn_agent"
                | "task_get"
                | "task_list"
                | "task_output"
                | "wait_agent"
                | "web_fetch"
                | "web_search"
        )
    }

    #[tokio::test]
    async fn mcp_resolved_secrets_skips_revoked_slots_for_restart_safe_load() -> Result<()> {
        let temp = tempdir()?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        let active = AuthSlotId::new("mcp.custom.active.SECRET_TOKEN");
        let revoked = AuthSlotId::new("mcp.custom.revoked.SECRET_TOKEN");
        auth_manager
            .store_generic_secret(active.clone(), "active-secret")
            .await?;
        auth_manager
            .store_generic_secret(revoked.clone(), "revoked-secret")
            .await?;
        auth_manager.revoke_slot_leases(&revoked)?;

        let resolved = mcp_resolved_secrets_from_auth_store(auth_manager.as_ref()).await?;
        assert_eq!(
            resolved.secret_values.get(&active.0).map(String::as_str),
            Some("active-secret")
        );
        assert!(!resolved.secret_values.contains_key(&revoked.0));
        assert!(resolved.revoked_secret_refs.contains(&revoked.0));
        Ok(())
    }

    #[test]
    fn audio_generation_backend_visibility_respects_route_capability_override() -> Result<()> {
        let route = ConfiguredModelRoute::new(
            "openai",
            ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-5.4", "test-key")),
        )
        .with_capabilities(RouteCapabilities {
            matrix_version: crate::ROUTE_CAPABILITY_MATRIX_VERSION,
            ..RouteCapabilities::default()
        });

        assert!(
            !has_audio_generation_backend(&[route]),
            "audio_generation=false should hide generate_audio even when the driver has a backend"
        );
        Ok(())
    }

    #[test]
    fn audio_generation_service_skips_routes_with_disabled_capability() -> Result<()> {
        let temp = tempdir()?;
        let assets = Arc::new(FileAssetStore::new(temp.path())?);
        let route = ConfiguredModelRoute::new(
            "openai",
            ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-5.4", "test-key")),
        )
        .with_capabilities(RouteCapabilities {
            matrix_version: crate::ROUTE_CAPABILITY_MATRIX_VERSION,
            ..RouteCapabilities::default()
        });

        let service = build_audio_generation_service(&[route], assets, Arc::new(NoopObserver))?;
        assert!(
            service.is_none(),
            "audio_generation=false should prevent backend registration"
        );
        Ok(())
    }

    #[test]
    fn transcription_service_skips_routes_with_disabled_capability() -> Result<()> {
        let route = ConfiguredModelRoute::new(
            "openai",
            ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-5.4", "test-key")),
        )
        .with_capabilities(RouteCapabilities {
            matrix_version: crate::ROUTE_CAPABILITY_MATRIX_VERSION,
            ..RouteCapabilities::default()
        });

        let service = super::build_transcription_service(&[route], &[], Arc::new(NoopObserver))?;
        assert!(
            service.is_none(),
            "transcription=false should prevent backend registration"
        );
        Ok(())
    }
}

/// Builds a production-ready Anthropic daemon service with default coding tools.
pub async fn build_anthropic_daemon(
    config: DaemonConfig,
    provider: kheish_runtime::AnthropicProviderConfig,
) -> Result<(DaemonService, TcpListener)> {
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(
        config,
        vec![ModelRouteConfig::Anthropic(provider)],
        Vec::new(),
        Vec::new(),
        auth_manager,
    )
    .await
}

/// Builds a production-ready OpenAI daemon service with default coding tools.
pub async fn build_openai_daemon(
    config: DaemonConfig,
    provider: kheish_runtime::OpenAiProviderConfig,
) -> Result<(DaemonService, TcpListener)> {
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(
        config,
        vec![ModelRouteConfig::OpenAi(provider)],
        Vec::new(),
        Vec::new(),
        auth_manager,
    )
    .await
}

/// Builds a production-ready OpenRouter daemon service with default coding tools.
pub async fn build_openrouter_daemon(
    config: DaemonConfig,
    provider: kheish_runtime::OpenRouterProviderConfig,
) -> Result<(DaemonService, TcpListener)> {
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(
        config,
        vec![ModelRouteConfig::OpenRouter(provider)],
        Vec::new(),
        Vec::new(),
        auth_manager,
    )
    .await
}

/// Builds a production-ready Google daemon service with default coding tools.
pub async fn build_google_daemon(
    config: DaemonConfig,
    provider: kheish_runtime::GoogleProviderConfig,
) -> Result<(DaemonService, TcpListener)> {
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(
        config,
        vec![ModelRouteConfig::Google(provider)],
        Vec::new(),
        Vec::new(),
        auth_manager,
    )
    .await
}

/// Builds a production-ready xAI daemon service with default coding tools.
pub async fn build_xai_daemon(
    config: DaemonConfig,
    provider: kheish_runtime::XAiProviderConfig,
) -> Result<(DaemonService, TcpListener)> {
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(
        config,
        vec![ModelRouteConfig::XAi(provider)],
        Vec::new(),
        Vec::new(),
        auth_manager,
    )
    .await
}

/// Builds a production-ready Anthropic daemon service with optional OpenAI fallback routing.
pub async fn build_anthropic_daemon_with_openai_fallback(
    config: DaemonConfig,
    provider: kheish_runtime::AnthropicProviderConfig,
    fallback: Option<kheish_runtime::OpenAiProviderConfig>,
) -> Result<(DaemonService, TcpListener)> {
    let mut routes = vec![ModelRouteConfig::Anthropic(provider)];
    if let Some(fallback) = fallback {
        routes.push(ModelRouteConfig::OpenAi(fallback));
    }
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(config, routes, Vec::new(), Vec::new(), auth_manager).await
}

/// Builds a production-ready OpenAI daemon service with optional Anthropic fallback routing.
pub async fn build_openai_daemon_with_anthropic_fallback(
    config: DaemonConfig,
    provider: kheish_runtime::OpenAiProviderConfig,
    fallback: Option<kheish_runtime::AnthropicProviderConfig>,
) -> Result<(DaemonService, TcpListener)> {
    let mut routes = vec![ModelRouteConfig::OpenAi(provider)];
    if let Some(fallback) = fallback {
        routes.push(ModelRouteConfig::Anthropic(fallback));
    }
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(config, routes, Vec::new(), Vec::new(), auth_manager).await
}

/// Builds a production-ready Google daemon service with optional OpenAI fallback routing.
pub async fn build_google_daemon_with_openai_fallback(
    config: DaemonConfig,
    provider: kheish_runtime::GoogleProviderConfig,
    fallback: Option<kheish_runtime::OpenAiProviderConfig>,
) -> Result<(DaemonService, TcpListener)> {
    let mut routes = vec![ModelRouteConfig::Google(provider)];
    if let Some(fallback) = fallback {
        routes.push(ModelRouteConfig::OpenAi(fallback));
    }
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(config, routes, Vec::new(), Vec::new(), auth_manager).await
}

/// Builds a production-ready xAI daemon service with optional OpenAI fallback routing.
pub async fn build_xai_daemon_with_openai_fallback(
    config: DaemonConfig,
    provider: kheish_runtime::XAiProviderConfig,
    fallback: Option<kheish_runtime::OpenAiProviderConfig>,
) -> Result<(DaemonService, TcpListener)> {
    let mut routes = vec![ModelRouteConfig::XAi(provider)];
    if let Some(fallback) = fallback {
        routes.push(ModelRouteConfig::OpenAi(fallback));
    }
    let auth_manager = AuthManager::new(config.state_root.join("auth/global-slots.json"))?;
    build_provider_daemon(config, routes, Vec::new(), Vec::new(), auth_manager).await
}
