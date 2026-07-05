//! Route-aware model routing for the daemon control plane.

use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use kheish_core::{ModelDriver, ModelRequest, ModelRequestKind, ModelTurn};
use kheish_runtime::{
    AnthropicProvider, AnthropicProviderConfig, DebugArtifact, DebugArtifactFormat, DebugControl,
    ExecutionScope, GoogleProvider, GoogleProviderConfig, ModelBudget, ModelRetryPolicy,
    ModelRuntime, OpenAiProvider, OpenAiProviderConfig, OpenRouterProvider,
    OpenRouterProviderConfig, RuntimeObserver, XAiProvider, XAiProviderConfig,
    current_execution_scope,
};
use kheish_types::{ModelGenerationConfig, ReasoningConfig, ReasoningEffort, Role};
use serde_json::json;

/// Resolves the effective route and model for one request-scoped execution.
pub(crate) trait DaemonModelControl: Send + Sync {
    fn current_model(&self) -> String;
    fn available_routes(&self) -> Vec<ResolvedModelRoute>;
    fn route_diagnostics(&self) -> Vec<RouteDiagnosticView> {
        Vec::new()
    }
    fn set_route(&self, provider: Option<&str>, model: String) -> Result<String>;
    fn resolve_route(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<ResolvedModelRoute>;

    /// Registers one route in the live inventory. Controls that do not support
    /// runtime route mutation reject the request.
    fn add_route(&self, _route: ConfiguredModelRoute) -> Result<()> {
        bail!("this daemon does not support adding model routes at runtime")
    }

    /// Removes one route from the live inventory. Returns whether it existed.
    /// Controls that do not support runtime route mutation reject the request.
    fn remove_route(&self, _route_id: &str) -> Result<bool> {
        bail!("this daemon does not support removing model routes at runtime")
    }
}

/// Current version of the daemon-visible route capability matrix.
pub const ROUTE_CAPABILITY_MATRIX_VERSION: u32 = 2;

/// Daemon-visible capabilities associated with one resolved route.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteCapabilities {
    /// Version of this capability matrix shape. `0` means a legacy pre-version payload.
    #[serde(default)]
    pub matrix_version: u32,
    /// Whether the route accepts multimodal user input, including image attachments.
    #[serde(default)]
    pub multimodal_input: bool,
    /// Whether the route can satisfy `web_search` through a native provider backend.
    #[serde(default)]
    pub native_web_search: bool,
    /// Whether the route exposes an image generation backend through daemon tools.
    #[serde(default)]
    pub image_generation: bool,
    /// Whether the route exposes an image editing backend through daemon tools.
    #[serde(default)]
    pub image_edit: bool,
    /// Whether the route exposes an audio generation backend through daemon tools.
    #[serde(default)]
    pub audio_generation: bool,
    /// Whether the route exposes an audio transcription backend through daemon workflows.
    #[serde(default)]
    pub transcription: bool,
}

/// The effective route selection pinned to one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModelRoute {
    /// Stable identifier for one configured daemon route.
    pub route_id: String,
    /// The underlying provider driver used by the route.
    pub provider: String,
    pub model: String,
    /// Optional daemon-managed auth reference bound to this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_ref: Option<String>,
    /// Daemon-visible capabilities associated with this route.
    pub capabilities: RouteCapabilities,
}

/// Severity for structured route diagnostics exposed through runtime status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// One read-only diagnostic about daemon model route configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDiagnosticView {
    pub severity: RouteDiagnosticSeverity,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    pub message: String,
}

/// Controls how one configured route validates requested model identifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSupportPolicy {
    /// Restrict the route to its built-in model family heuristics.
    #[default]
    Family,
    /// Accept any non-empty model string for the configured route.
    Any,
}

fn is_anthropic_model(model: &str) -> bool {
    let normalized = model.to_ascii_lowercase();
    normalized.contains("claude")
}

fn is_openai_model(model: &str) -> bool {
    let normalized = model.to_ascii_lowercase();
    normalized.starts_with("gpt") || normalized.starts_with("o1") || normalized.starts_with("o3")
}

fn is_gpt_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("gpt")
}

fn is_xai_model(model: &str) -> bool {
    let normalized = model.to_ascii_lowercase();
    normalized.starts_with("grok")
}

fn is_google_model(model: &str) -> bool {
    let normalized = model.to_ascii_lowercase();
    normalized.starts_with("gemini")
}

fn is_openrouter_model(model: &str) -> bool {
    let trimmed = model.trim();
    if trimmed.is_empty() || !trimmed.contains('/') {
        return false;
    }
    let vendor = trimmed
        .split_once('/')
        .map(|(vendor, _)| vendor.trim().to_ascii_lowercase())
        .unwrap_or_default();
    !matches!(
        vendor.as_str(),
        "anthropic" | "openai" | "google" | "xai" | "x-ai"
    )
}

/// Returns true when the request exposes the daemon image generation tool.
fn request_can_generate_images(request: &ModelRequest) -> bool {
    request
        .available_tools
        .iter()
        .any(|tool| tool.name == "generate_image")
}

/// Returns true when the latest user turn is asking the agent to create an image.
fn latest_user_turn_requests_image_generation(request: &ModelRequest) -> bool {
    let Some(content) = request
        .prompt
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.to_ascii_lowercase())
    else {
        return false;
    };

    let mentions_image_target = [
        "image",
        "photo",
        "illustration",
        "poster",
        "logo",
        "avatar",
        "wallpaper",
        "visuel",
        "visuelle",
    ]
    .iter()
    .any(|needle| content.contains(needle));
    if !mentions_image_target {
        return false;
    }

    [
        "generate",
        "generation",
        "create",
        "draw",
        "make",
        "render",
        "produce",
        "paint",
        "design",
        "génération",
        "génère",
        "générer",
        "genere",
        "generer",
        "crée",
        "cree",
        "créer",
        "creer",
        "dessine",
        "fais",
        "faire",
        "fabrique",
        "rends",
        "je veux une image",
        "j'aimerais une image",
        "i want an image",
    ]
    .iter()
    .any(|needle| content.contains(needle))
}

/// Enforces GPT image-generation planning quality without downgrading an explicit xhigh pin.
fn ensure_high_reasoning_for_generation(generation: &mut ModelGenerationConfig) {
    let reasoning = generation
        .reasoning
        .get_or_insert_with(ReasoningConfig::default);
    if !matches!(
        reasoning.effort,
        Some(ReasoningEffort::High | ReasoningEffort::Xhigh)
    ) {
        reasoning.effort = Some(ReasoningEffort::High);
    }
}

/// Applies daemon-level generation policy that depends on the resolved provider route.
fn apply_route_generation_policy(
    route: &DynamicModelRoute,
    effective_model: &str,
    request: &mut ModelRequest,
) {
    if request.kind != ModelRequestKind::MainLoop {
        return;
    }
    if route.provider_name() != "openai" || !is_gpt_model(effective_model) {
        return;
    }
    if !request_can_generate_images(request) || !latest_user_turn_requests_image_generation(request)
    {
        return;
    }
    ensure_high_reasoning_for_generation(&mut request.generation);
}

struct SwappableModelState {
    selected_model: String,
    drivers: HashMap<String, Arc<dyn ModelDriver>>,
}

#[derive(Clone, Debug)]
pub enum ModelRouteConfig {
    Anthropic(AnthropicProviderConfig),
    Google(GoogleProviderConfig),
    OpenAi(OpenAiProviderConfig),
    OpenRouter(OpenRouterProviderConfig),
    XAi(XAiProviderConfig),
}

impl ModelRouteConfig {
    pub(crate) fn provider_name(&self) -> &'static str {
        match self {
            Self::Anthropic(_) => "anthropic",
            Self::Google(_) => "google",
            Self::OpenAi(_) => "openai",
            Self::OpenRouter(_) => "openrouter",
            Self::XAi(_) => "xai",
        }
    }

    fn model_name(&self) -> &str {
        match self {
            Self::Anthropic(config) => &config.model,
            Self::Google(config) => &config.model,
            Self::OpenAi(config) => &config.model,
            Self::OpenRouter(config) => &config.model,
            Self::XAi(config) => &config.model,
        }
    }

    fn route_capabilities(&self) -> RouteCapabilities {
        match self {
            Self::Anthropic(_) => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: self.supports_image_generation_backend(),
                image_edit: self.supports_image_edit_backend(),
                audio_generation: self.supports_audio_generation_backend(),
                transcription: self.supports_transcription_backend(),
            },
            Self::Google(_) => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: false,
                image_generation: self.supports_image_generation_backend(),
                image_edit: self.supports_image_edit_backend(),
                audio_generation: self.supports_audio_generation_backend(),
                transcription: self.supports_transcription_backend(),
            },
            Self::OpenAi(_) => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: self.supports_image_generation_backend(),
                image_edit: self.supports_image_edit_backend(),
                audio_generation: self.supports_audio_generation_backend(),
                transcription: self.supports_transcription_backend(),
            },
            Self::OpenRouter(config) => openrouter_route_capabilities(config, &config.model)
                .unwrap_or_else(|| RouteCapabilities {
                    matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                    multimodal_input: true,
                    native_web_search: false,
                    image_generation: self.supports_image_generation_backend(),
                    image_edit: self.supports_image_edit_backend(),
                    audio_generation: self.supports_audio_generation_backend(),
                    transcription: self.supports_transcription_backend(),
                }),
            Self::XAi(_) => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: self.supports_image_generation_backend(),
                image_edit: self.supports_image_edit_backend(),
                audio_generation: self.supports_audio_generation_backend(),
                transcription: self.supports_transcription_backend(),
            },
        }
    }

    pub(crate) fn supports_image_generation_backend(&self) -> bool {
        matches!(
            self,
            Self::Google(_) | Self::OpenAi(_) | Self::OpenRouter(_) | Self::XAi(_)
        )
    }

    pub(crate) fn supports_image_edit_backend(&self) -> bool {
        matches!(
            self,
            Self::Google(_) | Self::OpenAi(_) | Self::OpenRouter(_) | Self::XAi(_)
        )
    }

    pub(crate) fn supports_audio_generation_backend(&self) -> bool {
        matches!(self, Self::OpenAi(_) | Self::OpenRouter(_))
    }

    pub(crate) fn supports_transcription_backend(&self) -> bool {
        matches!(self, Self::OpenAi(_) | Self::OpenRouter(_))
    }

    pub(crate) fn supported_capabilities(&self) -> RouteCapabilities {
        self.route_capabilities()
    }

    fn capabilities_for_model(&self, model: &str) -> RouteCapabilities {
        match self {
            Self::OpenRouter(config) => openrouter_route_capabilities(config, model)
                .unwrap_or_else(|| self.route_capabilities()),
            _ => self.route_capabilities(),
        }
    }

    fn supports_model(&self, model: &str) -> bool {
        match self {
            Self::Anthropic(_) => is_anthropic_model(model),
            Self::Google(_) => is_google_model(model),
            Self::OpenAi(_) => is_openai_model(model),
            Self::OpenRouter(_) => is_openrouter_model(model),
            Self::XAi(_) => is_xai_model(model),
        }
    }

    fn with_model(&self, model: String) -> Self {
        match self {
            Self::Anthropic(config) => {
                let mut cloned = config.clone();
                cloned.model = model;
                Self::Anthropic(cloned)
            }
            Self::Google(config) => {
                let mut cloned = config.clone();
                cloned.model = model;
                Self::Google(cloned)
            }
            Self::OpenAi(config) => {
                let mut cloned = config.clone();
                cloned.model = model;
                Self::OpenAi(cloned)
            }
            Self::OpenRouter(config) => {
                let mut cloned = config.clone();
                cloned.model = model;
                Self::OpenRouter(cloned)
            }
            Self::XAi(config) => {
                let mut cloned = config.clone();
                cloned.model = model;
                Self::XAi(cloned)
            }
        }
    }

    fn build_driver(
        &self,
        retry: &ModelRetryPolicy,
        budget: &ModelBudget,
        observer: Arc<dyn RuntimeObserver>,
        _debug: DebugControl,
    ) -> Result<Arc<dyn ModelDriver>> {
        match self {
            Self::Anthropic(config) => {
                let provider = AnthropicProvider::with_observer(config.clone(), observer.clone())?;
                Ok(Arc::new(ModelRuntime::new(
                    provider,
                    retry.clone(),
                    budget.clone(),
                    observer,
                )))
            }
            Self::Google(config) => {
                let provider = GoogleProvider::with_observer(config.clone(), observer.clone())?;
                Ok(Arc::new(ModelRuntime::new(
                    provider,
                    retry.clone(),
                    budget.clone(),
                    observer,
                )))
            }
            Self::OpenAi(config) => {
                let provider = OpenAiProvider::with_observer(config.clone(), observer.clone())?;
                Ok(Arc::new(ModelRuntime::new(
                    provider,
                    retry.clone(),
                    budget.clone(),
                    observer,
                )))
            }
            Self::OpenRouter(config) => {
                let provider = OpenRouterProvider::with_observer(config.clone(), observer.clone())?;
                Ok(Arc::new(ModelRuntime::new(
                    provider,
                    retry.clone(),
                    budget.clone(),
                    observer,
                )))
            }
            Self::XAi(config) => {
                let provider = XAiProvider::with_observer(config.clone(), observer.clone())?;
                Ok(Arc::new(ModelRuntime::new(
                    provider,
                    retry.clone(),
                    budget.clone(),
                    observer,
                )))
            }
        }
    }
}

fn openrouter_route_capabilities(
    config: &OpenRouterProviderConfig,
    model: &str,
) -> Option<RouteCapabilities> {
    let capabilities = config.capability_for_model(model)?;
    Some(RouteCapabilities {
        matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
        multimodal_input: capabilities.multimodal_input(),
        native_web_search: false,
        image_generation: capabilities.image_generation(),
        image_edit: capabilities.image_edit(),
        audio_generation: true,
        transcription: true,
    })
}

/// One configured daemon route with a stable public identifier.
#[derive(Clone, Debug)]
pub struct ConfiguredModelRoute {
    route_id: String,
    auth_ref: Option<String>,
    model_support: ModelSupportPolicy,
    base_capabilities: RouteCapabilities,
    capability_limit: Option<RouteCapabilities>,
    route: ModelRouteConfig,
}

impl ConfiguredModelRoute {
    /// Builds one legacy route whose public identifier matches the driver name.
    pub fn legacy(route: ModelRouteConfig) -> Self {
        let route_id = route.provider_name().to_string();
        let base_capabilities = route.route_capabilities();
        let model_support = if route.provider_name() == "openrouter" {
            ModelSupportPolicy::Any
        } else {
            ModelSupportPolicy::Family
        };
        Self {
            route_id,
            auth_ref: None,
            model_support,
            base_capabilities,
            capability_limit: None,
            route,
        }
    }

    /// Builds one configured route with a caller-selected stable identifier.
    pub fn new(route_id: impl Into<String>, route: ModelRouteConfig) -> Self {
        let base_capabilities = route.route_capabilities();
        let model_support = if route.provider_name() == "openrouter" {
            ModelSupportPolicy::Any
        } else {
            ModelSupportPolicy::Family
        };
        Self {
            route_id: route_id.into(),
            auth_ref: None,
            model_support,
            base_capabilities,
            capability_limit: None,
            route,
        }
    }

    /// Records the daemon-managed auth reference bound to this route.
    pub fn with_auth_ref(mut self, auth_ref: Option<String>) -> Self {
        self.auth_ref = auth_ref.filter(|value| !value.trim().is_empty());
        self
    }

    /// Replaces the model support policy used when resolving selectors.
    pub fn with_model_support(mut self, model_support: ModelSupportPolicy) -> Self {
        self.model_support = model_support;
        self
    }

    /// Replaces the externally visible route capabilities.
    pub fn with_capabilities(mut self, capabilities: RouteCapabilities) -> Self {
        self.capability_limit = Some(capabilities);
        self
    }

    /// Returns the stable public route identifier.
    pub fn route_id(&self) -> &str {
        &self.route_id
    }

    /// Returns the daemon-managed auth reference bound to this route, if any.
    pub fn auth_ref(&self) -> Option<&str> {
        self.auth_ref.as_deref()
    }

    /// Returns the underlying provider driver name.
    pub fn provider_name(&self) -> &'static str {
        self.route.provider_name()
    }

    /// Returns the route's current template model.
    pub fn model_name(&self) -> &str {
        self.route.model_name()
    }

    /// Returns the externally visible route capabilities.
    pub fn capabilities(&self) -> &RouteCapabilities {
        self.capability_limit
            .as_ref()
            .unwrap_or(&self.base_capabilities)
    }

    /// Returns the underlying provider route configuration.
    pub fn route_config(&self) -> &ModelRouteConfig {
        &self.route
    }

    /// Replaces the underlying provider route configuration.
    pub fn with_route_config(mut self, route: ModelRouteConfig) -> Self {
        self.base_capabilities = route.capabilities_for_model(route.model_name());
        self.route = route;
        self
    }

    fn capabilities_for_model(&self, model: &str) -> RouteCapabilities {
        let mut capabilities = self.route.capabilities_for_model(model);
        if let Some(limit) = &self.capability_limit {
            capabilities.multimodal_input &= limit.multimodal_input;
            capabilities.native_web_search &= limit.native_web_search;
            capabilities.image_generation &= limit.image_generation;
            capabilities.image_edit &= limit.image_edit;
            capabilities.audio_generation &= limit.audio_generation;
            capabilities.transcription &= limit.transcription;
        }
        capabilities
    }

    fn supports_model(&self, model: &str) -> bool {
        match self.model_support {
            ModelSupportPolicy::Family => self.route.supports_model(model),
            ModelSupportPolicy::Any => !model.trim().is_empty(),
        }
    }

    fn supports_explicit_model(&self, model: &str) -> bool {
        if self.provider_name() == "openrouter" {
            return !model.trim().is_empty();
        }
        self.supports_model(model)
    }

    fn with_model(&self, model: String) -> ModelRouteConfig {
        self.route.with_model(model)
    }

    fn build_driver(
        &self,
        retry: &ModelRetryPolicy,
        budget: &ModelBudget,
        observer: Arc<dyn RuntimeObserver>,
        debug: DebugControl,
    ) -> Result<Arc<dyn ModelDriver>> {
        self.route.build_driver(retry, budget, observer, debug)
    }
}

impl From<ModelRouteConfig> for ConfiguredModelRoute {
    fn from(value: ModelRouteConfig) -> Self {
        Self::legacy(value)
    }
}

pub(crate) struct DynamicModelRoute {
    template: ConfiguredModelRoute,
    state: Arc<RwLock<SwappableModelState>>,
    retry: ModelRetryPolicy,
    budget: ModelBudget,
    observer: Arc<dyn RuntimeObserver>,
    debug: DebugControl,
}

impl DynamicModelRoute {
    pub(crate) fn new(
        template: ConfiguredModelRoute,
        retry: ModelRetryPolicy,
        budget: ModelBudget,
        observer: Arc<dyn RuntimeObserver>,
        debug: DebugControl,
    ) -> Result<Arc<Self>> {
        let model = template.model_name().to_string();
        let driver = template.build_driver(&retry, &budget, observer.clone(), debug.clone())?;
        let mut drivers = HashMap::new();
        drivers.insert(model.clone(), driver);
        Ok(Arc::new(Self {
            template,
            state: Arc::new(RwLock::new(SwappableModelState {
                selected_model: model,
                drivers,
            })),
            retry,
            budget,
            observer,
            debug,
        }))
    }

    pub(crate) fn route_id(&self) -> &str {
        self.template.route_id()
    }

    pub(crate) fn provider_name(&self) -> &'static str {
        self.template.provider_name()
    }

    pub(crate) fn capabilities(&self) -> RouteCapabilities {
        self.template.capabilities_for_model(&self.current_model())
    }

    pub(crate) fn capabilities_for_model(&self, model: &str) -> RouteCapabilities {
        self.template.capabilities_for_model(model)
    }

    pub(crate) fn supports_model(&self, model: &str) -> bool {
        self.template.supports_model(model)
    }

    pub(crate) fn current_model(&self) -> String {
        self.state.read().selected_model.clone()
    }

    pub(crate) fn driver_for_model(&self, model: &str) -> Result<Arc<dyn ModelDriver>> {
        {
            let state = self.state.read();
            if let Some(driver) = state.drivers.get(model) {
                return Ok(driver.clone());
            }
        }

        let template = self.template.with_model(model.to_string());
        let driver = template.build_driver(
            &self.retry,
            &self.budget,
            self.observer.clone(),
            self.debug.clone(),
        )?;
        let mut state = self.state.write();
        state.drivers.insert(model.to_string(), driver.clone());
        Ok(driver)
    }

    pub(crate) fn set_selected_model(&self, model: &str) {
        self.state.write().selected_model = model.to_string();
    }
}

#[derive(Clone, Debug)]
struct ActiveModelSelection {
    route_id: String,
    model: String,
}

/// A route registry shared between the live [`RoutedModelDriver`] and its
/// [`RoutedModelControl`]. Both hold the same `Arc`, so a route added or removed
/// through the runtime API is visible to in-flight resolution immediately.
type SharedModelRoutes = Arc<RwLock<Vec<Arc<DynamicModelRoute>>>>;

pub(crate) struct RoutedModelDriver {
    routes: SharedModelRoutes,
    active_selection: Arc<RwLock<ActiveModelSelection>>,
}

impl RoutedModelDriver {
    fn new(routes: SharedModelRoutes, active_selection: Arc<RwLock<ActiveModelSelection>>) -> Self {
        Self {
            routes,
            active_selection,
        }
    }

    fn active_route(&self) -> Option<Arc<DynamicModelRoute>> {
        let active_route_id = self.active_selection.read().route_id.clone();
        let routes = self.routes.read();
        routes
            .iter()
            .find(|route| route.route_id() == active_route_id)
            .cloned()
            .or_else(|| routes.first().cloned())
    }

    fn select_route(
        &self,
        explicit_model: Option<&str>,
        scope: Option<&ExecutionScope>,
    ) -> Result<Arc<DynamicModelRoute>> {
        if let Some(route_id) = scope.and_then(|scope| scope.provider.as_deref()) {
            if let Some(route) = self
                .routes
                .read()
                .iter()
                .find(|route| route.route_id() == route_id)
                .cloned()
            {
                return Ok(route);
            }
            bail!(
                "daemon has no route configured for `{route_id}`; reload route configuration or update the pinned run/session route"
            );
        }
        if let Some(model) = explicit_model {
            if let Some(active_route) = self.active_route()
                && active_route.supports_model(model)
            {
                return Ok(active_route);
            }
            if let Some(route) = self
                .routes
                .read()
                .iter()
                .find(|route| route.supports_model(model))
                .cloned()
            {
                return Ok(route);
            }
        }
        self.active_route().ok_or_else(|| {
            anyhow!(
                "daemon has no model routes configured; add one via POST /v1/runtime/routes before running"
            )
        })
    }

    fn scoped_model(scope: Option<&ExecutionScope>) -> Option<String> {
        scope
            .and_then(|scope| scope.model.as_ref())
            .filter(|model| !model.trim().is_empty())
            .cloned()
    }

    fn default_model_for_route(
        &self,
        route: &DynamicModelRoute,
        scope: Option<&ExecutionScope>,
    ) -> String {
        if scope.and_then(|scope| scope.provider.as_deref()).is_some() {
            return route.current_model();
        }
        self.active_selection.read().model.clone()
    }
}

#[async_trait]
impl ModelDriver for RoutedModelDriver {
    async fn next_turn(&self, mut request: ModelRequest) -> Result<ModelTurn> {
        let scope = current_execution_scope();
        let explicit_model = request.generation.model.clone();
        let scoped_model = Self::scoped_model(scope.as_ref());
        let route_selector_model = explicit_model.as_deref().or(scoped_model.as_deref());
        let route = self.select_route(route_selector_model, scope.as_ref())?;
        let effective_model = explicit_model
            .or(scoped_model)
            .unwrap_or_else(|| self.default_model_for_route(&route, scope.as_ref()));
        if !route.template.supports_explicit_model(&effective_model) {
            bail!(
                "model `{}` is not compatible with route `{}`",
                effective_model,
                route.route_id()
            );
        }
        request.generation.model = Some(effective_model.clone());
        apply_route_generation_policy(&route, &effective_model, &mut request);
        record_route_resolution_debug(&route, &effective_model, &request, scope.as_ref());
        let driver = route.driver_for_model(&effective_model)?;
        driver.next_turn(request).await
    }
}

fn record_route_resolution_debug(
    route: &DynamicModelRoute,
    effective_model: &str,
    request: &ModelRequest,
    scope: Option<&ExecutionScope>,
) {
    let level = route
        .debug
        .level_for_run(scope.and_then(|scope| scope.run_id.as_deref()));
    if !level.is_enabled() {
        return;
    }
    route.observer.record_debug_artifact(DebugArtifact::new(
        level,
        Some(request.turn),
        None,
        "route-resolution",
        DebugArtifactFormat::Json,
        json!({
            "route_id": route.route_id(),
            "provider": route.provider_name(),
            "model": effective_model,
            "capabilities": route.capabilities_for_model(effective_model),
            "scope": {
                "provider": scope.and_then(|scope| scope.provider.as_deref()),
                "model": scope.and_then(|scope| scope.model.as_deref()),
            },
            "request_generation_model": request.generation.model.as_deref(),
        }),
    ));
}

pub(crate) struct RoutedModelControl {
    routes: SharedModelRoutes,
    active_selection: Arc<RwLock<ActiveModelSelection>>,
    retry: ModelRetryPolicy,
    budget: ModelBudget,
    observer: Arc<dyn RuntimeObserver>,
    debug: DebugControl,
}

impl RoutedModelControl {
    /// Builds the live driver and its control handle from an initial inventory.
    ///
    /// An empty inventory is allowed: the daemon boots in a degraded state with
    /// no routes, and the first route added through the runtime API becomes the
    /// default. Runs fail with a clear error until a route exists.
    pub(crate) fn new<R>(
        route_configs: Vec<R>,
        retry: ModelRetryPolicy,
        budget: ModelBudget,
        observer: Arc<dyn RuntimeObserver>,
        debug: DebugControl,
    ) -> Result<(RoutedModelDriver, Arc<Self>)>
    where
        R: Into<ConfiguredModelRoute>,
    {
        let mut routes = Vec::new();
        let mut seen_route_ids = HashSet::new();
        let mut active_selection = ActiveModelSelection {
            route_id: String::new(),
            model: String::new(),
        };
        for (index, configured) in route_configs.into_iter().map(Into::into).enumerate() {
            let route_id = configured.route_id().to_string();
            if !seen_route_ids.insert(route_id.clone()) {
                bail!("duplicate provider route `{route_id}`");
            }
            if index == 0 {
                active_selection.route_id = route_id;
                active_selection.model = configured.model_name().to_string();
            }
            routes.push(DynamicModelRoute::new(
                configured,
                retry.clone(),
                budget.clone(),
                observer.clone(),
                debug.clone(),
            )?);
        }
        let routes: SharedModelRoutes = Arc::new(RwLock::new(routes));
        let active_selection = Arc::new(RwLock::new(active_selection));
        let runtime = RoutedModelDriver::new(routes.clone(), active_selection.clone());
        let control = Arc::new(Self {
            routes,
            active_selection,
            retry,
            budget,
            observer,
            debug,
        });
        Ok((runtime, control))
    }

    fn active_route(&self) -> Option<Arc<DynamicModelRoute>> {
        let active_route_id = self.active_selection.read().route_id.clone();
        let routes = self.routes.read();
        routes
            .iter()
            .find(|route| route.route_id() == active_route_id)
            .cloned()
            .or_else(|| routes.first().cloned())
    }

    /// Registers one route in the shared registry, live for the next request.
    ///
    /// When the inventory was empty the new route becomes the default so runs
    /// resolve to it without an explicit selector.
    fn add_route(&self, configured: ConfiguredModelRoute) -> Result<()> {
        let route_id = configured.route_id().to_string();
        let model = configured.model_name().to_string();
        let route = DynamicModelRoute::new(
            configured,
            self.retry.clone(),
            self.budget.clone(),
            self.observer.clone(),
            self.debug.clone(),
        )?;
        let mut routes = self.routes.write();
        if routes.iter().any(|route| route.route_id() == route_id) {
            bail!("model route `{route_id}` already exists");
        }
        let becomes_default = routes.is_empty();
        routes.push(route);
        drop(routes);
        if becomes_default {
            *self.active_selection.write() = ActiveModelSelection { route_id, model };
        }
        Ok(())
    }

    /// Removes one route from the shared registry. Returns whether it existed.
    ///
    /// If the removed route was the active default, the default falls back to
    /// the first remaining route (or none when the inventory empties out).
    fn remove_route(&self, route_id: &str) -> Result<bool> {
        let mut routes = self.routes.write();
        let Some(index) = routes.iter().position(|route| route.route_id() == route_id) else {
            return Ok(false);
        };
        routes.remove(index);
        let mut selection = self.active_selection.write();
        if selection.route_id == route_id {
            match routes.first() {
                Some(route) => {
                    selection.route_id = route.route_id().to_string();
                    selection.model = route.current_model();
                }
                None => {
                    selection.route_id = String::new();
                    selection.model = String::new();
                }
            }
        }
        Ok(true)
    }
}

impl DaemonModelControl for RoutedModelControl {
    fn current_model(&self) -> String {
        self.active_selection.read().model.clone()
    }

    fn available_routes(&self) -> Vec<ResolvedModelRoute> {
        self.routes
            .read()
            .iter()
            .map(|route| ResolvedModelRoute {
                route_id: route.route_id().to_string(),
                provider: route.provider_name().to_string(),
                model: route.current_model(),
                auth_ref: route.template.auth_ref.clone(),
                capabilities: route.capabilities(),
            })
            .collect()
    }

    fn route_diagnostics(&self) -> Vec<RouteDiagnosticView> {
        let mut diagnostics = Vec::new();
        let routes = self.routes.read();
        if routes.is_empty() {
            diagnostics.push(RouteDiagnosticView {
                severity: RouteDiagnosticSeverity::Error,
                code: "route_inventory_empty".to_string(),
                route_id: None,
                message: "daemon has no model routes configured".to_string(),
            });
            return diagnostics;
        }

        let active_route_id = self.active_selection.read().route_id.clone();
        if !routes
            .iter()
            .any(|route| route.route_id() == active_route_id)
        {
            diagnostics.push(RouteDiagnosticView {
                severity: RouteDiagnosticSeverity::Error,
                code: "active_route_missing".to_string(),
                route_id: Some(active_route_id.clone()),
                message: format!(
                    "active route `{active_route_id}` is not present in route inventory"
                ),
            });
        }

        for route in routes.iter() {
            let model = route.current_model();
            if model.trim().is_empty() {
                diagnostics.push(RouteDiagnosticView {
                    severity: RouteDiagnosticSeverity::Error,
                    code: "route_model_empty".to_string(),
                    route_id: Some(route.route_id().to_string()),
                    message: format!("route `{}` has an empty selected model", route.route_id()),
                });
            } else if !route.supports_model(&model) {
                diagnostics.push(RouteDiagnosticView {
                    severity: RouteDiagnosticSeverity::Error,
                    code: "route_model_incompatible".to_string(),
                    route_id: Some(route.route_id().to_string()),
                    message: format!(
                        "selected model `{model}` is not compatible with route `{}`",
                        route.route_id()
                    ),
                });
            }
        }

        diagnostics
    }

    fn set_route(&self, provider: Option<&str>, model: String) -> Result<String> {
        let resolved = self.resolve_route(provider, Some(&model))?;
        let route = self
            .routes
            .read()
            .iter()
            .find(|route| route.route_id() == resolved.route_id.as_str())
            .cloned()
            .ok_or_else(|| anyhow!("daemon has no model route `{}`", resolved.route_id))?;
        route.driver_for_model(&model)?;
        route.set_selected_model(&model);
        *self.active_selection.write() = ActiveModelSelection {
            route_id: resolved.route_id,
            model: model.clone(),
        };
        Ok(model)
    }

    fn resolve_route(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<ResolvedModelRoute> {
        let active_route = self.active_route();
        let routes = self.routes.read();
        let route = match (provider, model) {
            (Some(provider), Some(model)) => {
                let route = routes
                    .iter()
                    .find(|route| route.route_id() == provider)
                    .cloned();
                match route {
                    Some(route) if route.template.supports_explicit_model(model) => Some(route),
                    Some(_) => bail!("model `{model}` is not compatible with route `{provider}`"),
                    None => None,
                }
            }
            (Some(provider), None) => routes
                .iter()
                .find(|route| route.route_id() == provider)
                .cloned(),
            (None, Some(model)) => {
                if let Some(active_route) = active_route.as_ref() {
                    if active_route.supports_model(model) {
                        Some(active_route.clone())
                    } else {
                        let matches = routes
                            .iter()
                            .filter(|route| route.supports_model(model))
                            .cloned()
                            .collect::<Vec<_>>();
                        match matches.len() {
                            0 => None,
                            1 => matches.into_iter().next(),
                            _ => {
                                let route_ids = matches
                                    .iter()
                                    .map(|route| route.route_id().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                bail!(
                                    "model `{model}` matches multiple routes ({route_ids}); use an explicit <route/model> selector"
                                );
                            }
                        }
                    }
                } else {
                    None
                }
            }
            (None, None) => active_route,
        };
        let route = match route {
            Some(route) => route,
            None if provider.is_some() || model.is_some() => match (provider, model) {
                (Some(provider), Some(_model)) => {
                    bail!("daemon has no route configured for `{provider}`")
                }
                (Some(provider), None) => {
                    bail!("daemon has no route configured for `{provider}`")
                }
                (None, Some(model)) => {
                    bail!("daemon has no route configured for model `{model}`")
                }
                (None, None) => unreachable!("explicit route guard requires provider or model"),
            },
            None => routes
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("daemon has no model routes configured"))?,
        };
        let model = model
            .map(str::to_string)
            .unwrap_or_else(|| route.current_model());
        let capabilities = route.capabilities_for_model(&model);
        Ok(ResolvedModelRoute {
            route_id: route.route_id().to_string(),
            provider: route.provider_name().to_string(),
            model: model.clone(),
            auth_ref: route.template.auth_ref.clone(),
            capabilities,
        })
    }

    fn add_route(&self, route: ConfiguredModelRoute) -> Result<()> {
        RoutedModelControl::add_route(self, route)
    }

    fn remove_route(&self, route_id: &str) -> Result<bool> {
        RoutedModelControl::remove_route(self, route_id)
    }
}

/// Provider drivers accepted by the runtime route-add API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteProviderKind {
    Anthropic,
    Google,
    OpenAi,
    OpenRouter,
    XAi,
}

impl RouteProviderKind {
    /// Parses one provider identifier from the runtime route-add request.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "google" | "gemini" => Some(Self::Google),
            "openai" => Some(Self::OpenAi),
            "openrouter" => Some(Self::OpenRouter),
            "xai" | "grok" => Some(Self::XAi),
            _ => None,
        }
    }
}

/// Builds one runtime-added route from an inline API key.
///
/// The key is embedded directly in the provider config; the daemon persists it
/// in the encrypted secret store separately and rebuilds the route from there at
/// boot. `auth_ref` records the secret slot so `/v1/runtime` surfaces it.
pub(crate) fn build_inline_api_key_route(
    route_id: &str,
    provider: RouteProviderKind,
    model: &str,
    api_key: &str,
    auth_ref: Option<String>,
) -> ConfiguredModelRoute {
    let route = match provider {
        RouteProviderKind::Anthropic => {
            ModelRouteConfig::Anthropic(AnthropicProviderConfig::new(model, api_key))
        }
        RouteProviderKind::Google => {
            ModelRouteConfig::Google(GoogleProviderConfig::new(model, api_key))
        }
        RouteProviderKind::OpenAi => {
            ModelRouteConfig::OpenAi(OpenAiProviderConfig::new(model, api_key))
        }
        RouteProviderKind::OpenRouter => {
            ModelRouteConfig::OpenRouter(OpenRouterProviderConfig::new(model, api_key))
        }
        RouteProviderKind::XAi => ModelRouteConfig::XAi(XAiProviderConfig::new(model, api_key)),
    };
    ConfiguredModelRoute::new(route_id.to_string(), route).with_auth_ref(auth_ref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kheish_runtime::OpenRouterModelCapabilities;

    fn test_openai_route() -> Arc<DynamicModelRoute> {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        DynamicModelRoute::new(
            ConfiguredModelRoute::legacy(ModelRouteConfig::OpenAi(OpenAiProviderConfig::new(
                "gpt-5.4",
                "test-openai-key",
            ))),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )
        .expect("test route should build")
    }

    fn test_anthropic_route() -> Arc<DynamicModelRoute> {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        DynamicModelRoute::new(
            ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                AnthropicProviderConfig::new("claude-opus-4-6", "test-anthropic-key"),
            )),
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )
        .expect("test route should build")
    }

    fn test_generate_image_tool() -> kheish_types::ToolDefinition {
        kheish_types::ToolDefinition {
            name: "generate_image".to_string(),
            description: "Generate an image".to_string(),
            input_schema: serde_json::Value::Null,
            allows_parallel: false,
        }
    }

    fn test_request(content: &str) -> ModelRequest {
        ModelRequest {
            kind: ModelRequestKind::MainLoop,
            conversation: kheish_types::ConversationKey {
                session_id: "test-session".to_string(),
                thread_id: None,
            },
            turn: 0,
            prompt: kheish_types::PromptProjection {
                messages: vec![kheish_types::MessageRecord::new(
                    "user-1",
                    kheish_types::Role::User,
                    content,
                )],
                ..Default::default()
            },
            provider_prompt: kheish_types::ProviderPrompt::default(),
            available_tools: vec![test_generate_image_tool()],
            generation: ModelGenerationConfig::default(),
        }
    }

    #[test]
    fn route_diagnostics_are_empty_for_clean_configured_routes() {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        let (_driver, control) = RoutedModelControl::new(
            vec![ConfiguredModelRoute::legacy(ModelRouteConfig::OpenAi(
                OpenAiProviderConfig::new("gpt-5.4", "test-openai-key"),
            ))],
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )
        .expect("test routed control should build");

        assert!(control.route_diagnostics().is_empty());
    }

    #[test]
    fn routed_model_control_rejects_duplicate_route_ids() {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        let error = match RoutedModelControl::new(
            vec![
                ConfiguredModelRoute::new(
                    "openai",
                    ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-5.4", "key-1")),
                ),
                ConfiguredModelRoute::new(
                    "openai",
                    ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-5.4-mini", "key-2")),
                ),
            ],
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        ) {
            Ok(_) => panic!("duplicate route ids should fail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("duplicate provider route `openai`")
        );
    }

    #[test]
    fn scoped_missing_route_is_not_rerouted_to_active_route() {
        let driver = RoutedModelDriver::new(
            Arc::new(RwLock::new(vec![
                test_anthropic_route(),
                test_openai_route(),
            ])),
            Arc::new(RwLock::new(ActiveModelSelection {
                route_id: "anthropic".to_string(),
                model: "claude-opus-4-6".to_string(),
            })),
        );
        let scope = ExecutionScope {
            provider: Some("missing".to_string()),
            model: Some("gpt-5.4".to_string()),
            ..ExecutionScope::default()
        };

        let error = match driver.select_route(Some("gpt-5.4"), Some(&scope)) {
            Ok(route) => panic!(
                "missing scoped route unexpectedly selected {}",
                route.route_id()
            ),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("daemon has no route configured for `missing`")
        );
    }

    #[test]
    fn route_capabilities_deserialize_legacy_and_expose_v2_media_flags() -> Result<()> {
        let legacy = serde_json::from_value::<RouteCapabilities>(json!({
            "multimodal_input": true,
            "native_web_search": true,
            "image_generation": true,
            "image_edit": false
        }))?;
        assert_eq!(legacy.matrix_version, 0);
        assert!(!legacy.audio_generation);
        assert!(!legacy.transcription);

        let openai =
            ModelRouteConfig::OpenAi(OpenAiProviderConfig::new("gpt-5.4", "test-openai-key"))
                .supported_capabilities();
        assert_eq!(openai.matrix_version, ROUTE_CAPABILITY_MATRIX_VERSION);
        assert!(openai.transcription);
        assert!(openai.audio_generation);

        let openrouter = ModelRouteConfig::OpenRouter(OpenRouterProviderConfig::new(
            "openai/gpt-5.4-mini",
            "test-openrouter-key",
        ))
        .supported_capabilities();
        assert!(openrouter.transcription);
        assert!(openrouter.audio_generation);

        let google =
            ModelRouteConfig::Google(GoogleProviderConfig::new("gemini-2.5-flash", "test-key"))
                .supported_capabilities();
        assert!(google.multimodal_input);
        assert!(!google.native_web_search);
        assert!(google.image_generation);
        assert!(google.image_edit);
        assert!(!google.audio_generation);
        assert!(!google.transcription);

        let xai = ModelRouteConfig::XAi(XAiProviderConfig::new(
            "grok-4-fast-reasoning",
            "test-xai-key",
        ))
        .supported_capabilities();
        assert!(xai.native_web_search);
        assert!(xai.image_generation);
        assert!(xai.image_edit);
        assert!(!xai.audio_generation);
        assert!(!xai.transcription);
        Ok(())
    }

    #[test]
    fn openrouter_route_capabilities_follow_discovered_model_matrix() -> Result<()> {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            "vendor/text-only".to_string(),
            OpenRouterModelCapabilities {
                tools: true,
                structured_output: true,
                text_input: true,
                text_output: true,
                ..OpenRouterModelCapabilities::default()
            },
        );
        capabilities.insert(
            "vendor/media".to_string(),
            OpenRouterModelCapabilities {
                tools: true,
                structured_output: true,
                text_input: true,
                image_input: true,
                audio_input: true,
                text_output: true,
                image_output: true,
                audio_output: true,
            },
        );
        let route = ConfiguredModelRoute::new(
            "openrouter",
            ModelRouteConfig::OpenRouter(
                OpenRouterProviderConfig::new("vendor/text-only", "test-openrouter-key")
                    .with_model_capabilities(capabilities),
            ),
        );

        let default_caps = route.capabilities_for_model("vendor/text-only");
        assert!(!default_caps.multimodal_input);
        assert!(!default_caps.image_generation);
        assert!(default_caps.audio_generation);
        assert!(default_caps.transcription);

        let media_caps = route.capabilities_for_model("vendor/media");
        assert!(media_caps.multimodal_input);
        assert!(media_caps.image_generation);
        assert!(media_caps.image_edit);
        assert!(media_caps.audio_generation);
        assert!(media_caps.transcription);
        Ok(())
    }

    #[test]
    fn openrouter_defaults_to_any_model_support_for_vendor_prefixed_models() -> Result<()> {
        let route = ConfiguredModelRoute::new(
            "openrouter",
            ModelRouteConfig::OpenRouter(OpenRouterProviderConfig::new(
                "openai/gpt-5.4-mini",
                "test-openrouter-key",
            )),
        );
        assert!(route.supports_model("openai/gpt-5.4-mini"));
        assert!(route.supports_model("anthropic/claude-sonnet-4"));
        Ok(())
    }

    #[test]
    fn route_policy_forces_high_reasoning_for_gpt_image_generation_requests() {
        let route = test_openai_route();
        let mut request = test_request("Generate an image of a precise red cube.");

        apply_route_generation_policy(&route, "gpt-5.4", &mut request);

        assert_eq!(
            request
                .generation
                .reasoning
                .and_then(|reasoning| reasoning.effort),
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn route_policy_preserves_xhigh_reasoning_for_gpt_image_generation_requests() {
        let route = test_openai_route();
        let mut request = test_request("Create an image of a precise blue cube.");
        request.generation.reasoning = Some(ReasoningConfig {
            effort: Some(ReasoningEffort::Xhigh),
            summary: None,
            budget_tokens: None,
            interleaved: false,
        });

        apply_route_generation_policy(&route, "gpt-5.4", &mut request);

        assert_eq!(
            request
                .generation
                .reasoning
                .and_then(|reasoning| reasoning.effort),
            Some(ReasoningEffort::Xhigh)
        );
    }

    #[test]
    fn route_policy_does_not_force_reasoning_for_non_generation_or_non_gpt_routes() {
        let openai = test_openai_route();
        let anthropic = test_anthropic_route();
        let mut image_analysis = test_request("Analyze this attached image carefully.");
        let mut anthropic_image = test_request("Generate an image of a green cube.");

        apply_route_generation_policy(&openai, "gpt-5.4", &mut image_analysis);
        apply_route_generation_policy(&anthropic, "claude-opus-4-6", &mut anthropic_image);

        assert!(image_analysis.generation.reasoning.is_none());
        assert!(anthropic_image.generation.reasoning.is_none());
    }

    #[test]
    fn available_routes_expose_bound_auth_refs() -> Result<()> {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        let (_driver, control) = RoutedModelControl::new(
            vec![
                ConfiguredModelRoute::new(
                    "openrouter",
                    ModelRouteConfig::OpenAi(OpenAiProviderConfig::new(
                        "openai/gpt-5.4-mini",
                        "test-openrouter-key",
                    )),
                )
                .with_auth_ref(Some("openrouter.primary".to_string()))
                .with_model_support(ModelSupportPolicy::Any),
                ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                    AnthropicProviderConfig::new("claude-opus-4-6", "test-anthropic-key"),
                )),
            ],
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )?;

        let routes = control.available_routes();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].route_id, "openrouter");
        assert_eq!(routes[0].auth_ref.as_deref(), Some("openrouter.primary"));
        assert_eq!(routes[1].route_id, "anthropic");
        assert!(routes[1].auth_ref.is_none());
        Ok(())
    }

    #[test]
    fn resolve_route_keeps_auth_ref_on_explicit_route_selection() -> Result<()> {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        let (_driver, control) = RoutedModelControl::new(
            vec![
                ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                    AnthropicProviderConfig::new("claude-opus-4-6", "test-anthropic-key"),
                )),
                ConfiguredModelRoute::new(
                    "openrouter",
                    ModelRouteConfig::OpenAi(OpenAiProviderConfig::new(
                        "openai/gpt-5.4-mini",
                        "test-openrouter-key",
                    )),
                )
                .with_auth_ref(Some("openrouter.primary".to_string()))
                .with_model_support(ModelSupportPolicy::Any),
            ],
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )?;

        let resolved = control.resolve_route(Some("openrouter"), Some("openai/gpt-5.4-mini"))?;
        assert_eq!(resolved.route_id, "openrouter");
        assert_eq!(resolved.auth_ref.as_deref(), Some("openrouter.primary"));
        Ok(())
    }

    #[test]
    fn openrouter_any_support_accepts_vendor_prefixed_models_without_false_diagnostics()
    -> Result<()> {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        let (_driver, control) = RoutedModelControl::new(
            vec![
                ConfiguredModelRoute::legacy(ModelRouteConfig::OpenRouter(
                    OpenRouterProviderConfig::new("openai/gpt-5.4-mini", "test-openrouter-key"),
                )),
                ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                    AnthropicProviderConfig::new("claude-opus-4-6", "test-anthropic-key"),
                )),
            ],
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )?;

        let implicit = control.resolve_route(None, Some("anthropic/claude-sonnet-4"))?;
        assert_eq!(implicit.route_id, "openrouter");
        assert_eq!(implicit.provider, "openrouter");
        let resolved =
            control.resolve_route(Some("openrouter"), Some("anthropic/claude-sonnet-4"))?;
        assert_eq!(resolved.route_id, "openrouter");
        assert_eq!(resolved.provider, "openrouter");
        assert!(control.route_diagnostics().is_empty());
        Ok(())
    }

    #[test]
    fn openrouter_family_accepts_non_native_vendor_prefixes_without_route_prefix() -> Result<()> {
        let observer = Arc::new(kheish_runtime::NoopObserver);
        let debug = DebugControl::new(kheish_runtime::DebugCaptureLevel::Off);
        let (_driver, control) = RoutedModelControl::new(
            vec![
                ConfiguredModelRoute::legacy(ModelRouteConfig::OpenRouter(
                    OpenRouterProviderConfig::new("meta/llama-4-scout", "test-openrouter-key"),
                )),
                ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                    AnthropicProviderConfig::new("claude-opus-4-6", "test-anthropic-key"),
                )),
            ],
            ModelRetryPolicy::default(),
            ModelBudget::default(),
            observer,
            debug,
        )?;

        let resolved = control.resolve_route(None, Some("meta/llama-4-scout"))?;
        assert_eq!(resolved.route_id, "openrouter");
        assert_eq!(resolved.provider, "openrouter");
        Ok(())
    }
}
