//! Serve-specific route, auth, and provider resolution helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::ValueEnum;
use tracing::warn;

use kheish_auth::{
    AnthropicAuthBackend, AuthManager, AuthMode, AuthProvider, AuthSlotId,
    DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL, DEFAULT_CLAUDE_CODE_CLIENT_ID,
    DEFAULT_OPENAI_CODEX_API_BASE_URL, DEFAULT_ROUTE_LEASE_TTL_MS, ExecutionCredentialContext,
    FileAuthStore, OpenAiAuthBackend, RequestAuthProvider, ResolvedAuthMaterial,
    default_claude_code_credentials_path,
};
use kheish_daemon::{
    AdditionalImageBackendConfig, AdditionalTranscriptionBackendConfig, ConfiguredModelRoute,
    ControlPlaneAuthConfig, ControlPlaneAuthTokenFiles, ControlPlaneCorsConfig, ModelRouteConfig,
    ModelSupportPolicy, RouteCapabilities, is_loopback_control_plane_origin,
};
use kheish_runtime::{
    AnthropicProviderConfig, GoogleProviderConfig, OpenAiProviderConfig, OpenRouterProviderConfig,
    XAiProviderConfig, current_execution_scope, fetch_openrouter_model_capabilities,
    resolve_google_image_model, resolve_openai_image_model, resolve_xai_image_model,
};

use crate::cli::{
    default_codex_auth_path, ensure_secret_manager_master_key_configured, global_auth_store_path,
    read_secret_arg,
};
use crate::route_file::{
    RouteFileAnthropicAuthSource, RouteFileDriver, RouteFileEntry, RouteFileOpenAiAuthSource,
    RoutesFileConfig,
};
use crate::{
    AnthropicAuthSourceArg, DEFAULT_ANTHROPIC_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_OPENAI_MODEL,
    DEFAULT_OPENROUTER_MODEL, DEFAULT_XAI_MODEL, HttpAuthModeArg, OpenAiAuthSourceArg,
    ProviderKind, ResolvedAnthropicAuthSource, ResolvedOpenAiAuthSource, ServeArgs,
};

#[derive(Clone)]
enum ScopedAuthMaterialSource {
    Slot(AuthSlotId),
    Inline(ResolvedAuthMaterial),
}

#[derive(Clone)]
struct ScopedRequestAuthProvider {
    manager: Arc<AuthManager>,
    route_id: String,
    source: ScopedAuthMaterialSource,
}

impl ScopedRequestAuthProvider {
    fn new_slot(
        manager: Arc<AuthManager>,
        slot_id: AuthSlotId,
        route_id: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            route_id: route_id.into(),
            source: ScopedAuthMaterialSource::Slot(slot_id),
        }
    }

    fn new_inline(
        manager: Arc<AuthManager>,
        route_id: impl Into<String>,
        material: ResolvedAuthMaterial,
    ) -> Self {
        Self {
            manager,
            route_id: route_id.into(),
            source: ScopedAuthMaterialSource::Inline(material),
        }
    }

    fn context(&self) -> ExecutionCredentialContext {
        current_execution_scope()
            .map(|scope| ExecutionCredentialContext {
                session_id: Some(scope.session_id).filter(|value| !value.is_empty()),
                agent_id: scope.agent_id,
                run_id: scope.run_id,
                principal_id: scope.principal_id,
                parent_principal_id: scope.parent_principal_id,
                delegation_id: scope.delegation_id,
                credential_scope: scope.credential_scope.normalized(),
            })
            .unwrap_or_default()
    }

    fn authorize_inline_route(&self) -> Result<kheish_auth::CredentialLease> {
        let synthetic_slot = AuthSlotId::new(format!("inline-route:{}", self.route_id));
        let grant = self.manager.broker().authorize_route(
            &synthetic_slot,
            &self.route_id,
            &self.context(),
        )?;
        self.manager
            .broker()
            .issue_route_lease(&grant, DEFAULT_ROUTE_LEASE_TTL_MS)
    }
}

#[async_trait]
impl RequestAuthProvider for ScopedRequestAuthProvider {
    async fn resolve(&self) -> Result<kheish_auth::ResolvedAuthMaterial> {
        match &self.source {
            ScopedAuthMaterialSource::Slot(slot_id) => {
                self.manager
                    .resolve_brokered(slot_id, &self.route_id, &self.context(), false)
                    .await
            }
            ScopedAuthMaterialSource::Inline(material) => {
                let mut material = material.clone();
                let lease = self.authorize_inline_route()?;
                material.grant_id = Some(lease.grant_id);
                material.lease_id = Some(lease.id);
                Ok(material)
            }
        }
    }

    async fn refresh(&self) -> Result<kheish_auth::ResolvedAuthMaterial> {
        match &self.source {
            ScopedAuthMaterialSource::Slot(slot_id) => {
                self.manager
                    .resolve_brokered(slot_id, &self.route_id, &self.context(), true)
                    .await
            }
            ScopedAuthMaterialSource::Inline(material) => {
                let mut material = material.clone();
                let lease = self.authorize_inline_route()?;
                material.grant_id = Some(lease.grant_id);
                material.lease_id = Some(lease.id);
                Ok(material)
            }
        }
    }

    async fn ensure_active(&self, material: &kheish_auth::ResolvedAuthMaterial) -> Result<()> {
        self.manager.ensure_resolved_material_active(material)
    }
}

fn brokered_request_provider(
    manager: Arc<AuthManager>,
    slot_id: AuthSlotId,
    route_id: impl Into<String>,
) -> Arc<dyn RequestAuthProvider> {
    Arc::new(ScopedRequestAuthProvider::new_slot(
        manager, slot_id, route_id,
    ))
}

fn inline_request_provider(
    manager: Arc<AuthManager>,
    route_id: impl Into<String>,
    material: ResolvedAuthMaterial,
) -> Arc<dyn RequestAuthProvider> {
    Arc::new(ScopedRequestAuthProvider::new_inline(
        manager, route_id, material,
    ))
}

fn bearer_auth_material(api_key: impl Into<String>) -> ResolvedAuthMaterial {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {}", api_key.into()),
    );
    ResolvedAuthMaterial {
        headers,
        base_url_override: None,
        grant_id: None,
        lease_id: None,
    }
}

fn header_auth_material(header: &str, value: impl Into<String>) -> ResolvedAuthMaterial {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(header.to_string(), value.into());
    ResolvedAuthMaterial {
        headers,
        base_url_override: None,
        grant_id: None,
        lease_id: None,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::{ScopedRequestAuthProvider, bearer_auth_material};
    use kheish_auth::{AuthManager, RequestAuthProvider};
    use kheish_runtime::{ExecutionScope, scope_execution};
    use kheish_types::CredentialScope;

    #[tokio::test]
    async fn inline_route_auth_provider_issues_route_leases_for_subject_status() -> Result<()> {
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth.json"))?;
        let provider = ScopedRequestAuthProvider::new_inline(
            manager.clone(),
            "openai",
            bearer_auth_material("sk-test"),
        );

        let material = scope_execution(
            ExecutionScope {
                session_id: "inline-route-session".to_string(),
                principal_id: Some("session:inline-route-session".to_string()),
                credential_scope: CredentialScope {
                    route_allow: vec!["openai".to_string()],
                    ..CredentialScope::default()
                },
                ..ExecutionScope::default()
            },
            CancellationToken::new(),
            async { provider.resolve().await },
        )
        .await?;

        assert!(material.grant_id.is_some());
        let status = manager
            .subject_status("session:inline-route-session")
            .expect("inline route should register a broker subject");
        assert_eq!(status.active_route_lease_ids.len(), 1);
        Ok(())
    }

    // The full control-plane auth resolution matrix — non-loopback refusals (mode none and auto),
    // bearer/admin token handling, duplicate-token rejection, and Auto+loopback fallback — is
    // covered by the `control_plane_auth_*` tests in `main.rs`. This adds the one branch they miss:
    // `--http-auth-mode none` on a loopback bind, which is also the second site that now emits the
    // "control-plane auth DISABLED" operator warning.
    #[test]
    fn control_plane_auth_disabled_on_loopback_with_mode_none() {
        use clap::Parser;
        let cli = crate::Cli::parse_from([
            "kheish-daemon",
            "serve",
            "--bind",
            "127.0.0.1:4000",
            "--http-auth-mode",
            "none",
        ]);
        let args = match cli.command {
            Some(crate::Command::Serve(args)) => args,
            _ => panic!("expected the serve subcommand"),
        };
        let config = super::resolve_control_plane_auth_config(&args)
            .expect("mode none on a loopback bind should be allowed");
        assert!(
            !config.is_enabled(),
            "mode none on loopback must disable control-plane auth"
        );
    }
}

/// Builds a disabled control-plane auth config and emits an operator warning.
///
/// Auth is only ever disabled on a loopback bind — non-loopback binds without an admin token are
/// refused before reaching here — but an unauthenticated control plane still grants full admin
/// access to every process that can reach the bind, so surface it loudly rather than silently.
fn disabled_control_plane_auth_with_warning(bind: std::net::SocketAddr) -> ControlPlaneAuthConfig {
    warn!(
        %bind,
        "daemon control-plane authentication is DISABLED; every client able to reach this bind has \
         full admin access. This is only intended for trusted loopback use. Set --http-admin-token \
         (optionally with --http-readonly-token) to require bearer authentication."
    );
    ControlPlaneAuthConfig::disabled()
}

/// Resolves the HTTP control-plane auth policy for one daemon instance.
pub(crate) fn resolve_control_plane_auth_config(
    args: &ServeArgs,
) -> Result<ControlPlaneAuthConfig> {
    let admin_token = read_secret_arg(
        args.http_admin_token.clone(),
        args.http_admin_token_file.as_deref(),
        "--http-admin-token",
        "--http-admin-token-file",
    )?;
    let read_only_token = read_secret_arg(
        args.http_readonly_token.clone(),
        args.http_readonly_token_file.as_deref(),
        "--http-readonly-token",
        "--http-readonly-token-file",
    )?;
    let bind_is_loopback = args.bind.ip().is_loopback();

    match args.http_auth_mode {
        HttpAuthModeArg::None => {
            if !bind_is_loopback {
                bail!(
                    "refusing to expose daemon control-plane on non-loopback bind {} with --http-auth-mode none; configure --http-admin-token",
                    args.bind
                );
            }
            Ok(disabled_control_plane_auth_with_warning(args.bind))
        }
        HttpAuthModeArg::Bearer => {
            let admin_token = admin_token.ok_or_else(|| {
                anyhow!(
                    "--http-auth-mode bearer requires --http-admin-token or --http-admin-token-file"
                )
            })?;
            validate_distinct_control_plane_tokens(&admin_token, read_only_token.as_deref())?;
            Ok(ControlPlaneAuthConfig {
                admin_token: Some(admin_token),
                read_only_token,
            })
        }
        HttpAuthModeArg::Auto => match admin_token {
            Some(admin_token) => {
                validate_distinct_control_plane_tokens(&admin_token, read_only_token.as_deref())?;
                Ok(ControlPlaneAuthConfig {
                    admin_token: Some(admin_token),
                    read_only_token,
                })
            }
            None if read_only_token.is_some() => {
                bail!("--http-readonly-token requires --http-admin-token or --http-auth-mode none")
            }
            None if bind_is_loopback => Ok(disabled_control_plane_auth_with_warning(args.bind)),
            None => bail!(
                "refusing to expose daemon control-plane on non-loopback bind {} without HTTP auth; configure --http-admin-token",
                args.bind
            ),
        },
    }
}

fn validate_distinct_control_plane_tokens(
    admin_token: &str,
    read_only_token: Option<&str>,
) -> Result<()> {
    if read_only_token.is_some_and(|read_only_token| read_only_token == admin_token) {
        bail!(
            "--http-readonly-token must be distinct from --http-admin-token; identical tokens make read-only/admin authorization ambiguous"
        );
    }
    Ok(())
}

/// Returns token-file sources used for live control-plane token rotation.
#[must_use]
pub(crate) fn control_plane_auth_token_files(args: &ServeArgs) -> ControlPlaneAuthTokenFiles {
    ControlPlaneAuthTokenFiles {
        admin_token_file: args.http_admin_token_file.clone(),
        read_only_token_file: args.http_readonly_token_file.clone(),
    }
}

/// Resolves the browser CORS policy for the daemon control-plane API.
pub(crate) fn resolve_control_plane_cors_config(
    args: &ServeArgs,
) -> Result<ControlPlaneCorsConfig> {
    if args.http_cors_allow_origins.is_empty() {
        return Ok(ControlPlaneCorsConfig::loopback());
    }

    let mut allowed_origins = Vec::new();
    for origin in &args.http_cors_allow_origins {
        let origin = origin.trim();
        if origin.is_empty() {
            bail!("--http-cors-allow-origin cannot contain an empty origin");
        }
        if origin == "*" {
            bail!("--http-cors-allow-origin does not accept wildcard origins");
        }
        if !is_loopback_control_plane_origin(origin) {
            bail!(
                "--http-cors-allow-origin must be an exact http(s) loopback origin without a path: {origin}"
            );
        }
        if !allowed_origins
            .iter()
            .any(|allowed_origin| allowed_origin == origin)
        {
            allowed_origins.push(origin.to_string());
        }
    }

    Ok(ControlPlaneCorsConfig::exact(allowed_origins))
}

/// Resolves the configured model-route inventory for `serve`.
pub(crate) async fn resolve_route_inventory(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Vec<ConfiguredModelRoute>> {
    if let Some(path) = args.routes_file.as_deref() {
        return resolve_route_inventory_from_file(args, path, manager).await;
    }
    resolve_legacy_route_inventory(args, manager).await
}

/// Resolves the additional image backends enabled for the daemon.
pub(crate) fn resolve_additional_image_backends(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Vec<AdditionalImageBackendConfig>> {
    let Some(request) = resolve_requested_image_backend(args)? else {
        return Ok(Vec::new());
    };
    let include_generic = request.provider == args.provider;
    let api_key = resolve_provider_image_api_key(request.provider, args, include_generic)
        .ok_or_else(|| {
            anyhow!(
                "missing {} for image backend",
                request.provider.api_key_env_key()
            )
        })?;
    let backend = match request.provider {
        ProviderKind::Anthropic => {
            bail!("anthropic does not support dedicated image backends")
        }
        ProviderKind::Google => {
            let mut config = GoogleProviderConfig::new(
                resolve_google_image_model(request.model.as_deref().unwrap_or_default()),
                api_key,
            );
            config.request_auth_provider = Some(inline_request_provider(
                manager.clone(),
                "google",
                header_auth_material("x-goog-api-key", config.api_key.clone().unwrap_or_default()),
            ));
            if let Some(base_url) = resolve_provider_image_base_url(ProviderKind::Google, args) {
                config.base_url = base_url;
            }
            AdditionalImageBackendConfig::google(config)
        }
        ProviderKind::Openai => {
            let mut config = OpenAiProviderConfig::new(
                resolve_openai_image_model(request.model.as_deref().unwrap_or_default()),
                api_key,
            );
            config.request_auth_provider = Some(inline_request_provider(
                manager.clone(),
                "openai",
                bearer_auth_material(config.api_key.clone().unwrap_or_default()),
            ));
            if let Some(base_url) = resolve_provider_image_base_url(ProviderKind::Openai, args) {
                config.base_url = base_url;
            }
            config.organization = args
                .openai_organization
                .clone()
                .or_else(|| std::env::var("OPENAI_ORGANIZATION").ok());
            config.project = args
                .openai_project
                .clone()
                .or_else(|| std::env::var("OPENAI_PROJECT").ok());
            AdditionalImageBackendConfig::openai(config)
        }
        ProviderKind::Openrouter => {
            bail!("openrouter does not support dedicated image backends yet")
        }
        ProviderKind::Xai => {
            let mut config = XAiProviderConfig::new(
                resolve_xai_image_model(request.model.as_deref().unwrap_or_default()),
                api_key,
            );
            config.request_auth_provider = Some(inline_request_provider(
                manager.clone(),
                "xai",
                bearer_auth_material(config.api_key.clone().unwrap_or_default()),
            ));
            if let Some(base_url) = resolve_provider_image_base_url(ProviderKind::Xai, args) {
                config.base_url = base_url;
            }
            AdditionalImageBackendConfig::xai(config)
        }
    };
    Ok(vec![backend])
}

/// Resolves the additional transcription backends enabled for the daemon.
pub(crate) fn resolve_additional_transcription_backends(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Vec<AdditionalTranscriptionBackendConfig>> {
    let Some(request) = resolve_requested_transcription_backend(args)? else {
        return Ok(Vec::new());
    };
    let include_generic = request.provider == args.provider;
    let api_key = resolve_provider_transcription_api_key(request.provider, args, include_generic)
        .ok_or_else(|| {
        anyhow!(
            "missing {} for transcription backend",
            request.provider.api_key_env_key()
        )
    })?;
    let backend = match request.provider {
        ProviderKind::Openai => {
            let mut config = OpenAiProviderConfig::new(
                request
                    .model
                    .clone()
                    .unwrap_or_else(|| "gpt-4o-transcribe".to_string()),
                api_key,
            );
            config.request_auth_provider = Some(inline_request_provider(
                manager,
                "openai",
                bearer_auth_material(config.api_key.clone().unwrap_or_default()),
            ));
            if let Some(base_url) =
                resolve_provider_transcription_base_url(ProviderKind::Openai, args)
            {
                config.base_url = base_url;
            }
            config.organization = args
                .openai_organization
                .clone()
                .or_else(|| std::env::var("OPENAI_ORGANIZATION").ok());
            config.project = args
                .openai_project
                .clone()
                .or_else(|| std::env::var("OPENAI_PROJECT").ok());
            AdditionalTranscriptionBackendConfig::openai(config)
        }
        ProviderKind::Openrouter => {
            let mut config = OpenRouterProviderConfig::new(
                request
                    .model
                    .clone()
                    .unwrap_or_else(|| "openai/gpt-4o-mini-transcribe".to_string()),
                api_key,
            );
            config.request_auth_provider = Some(inline_request_provider(
                manager,
                "openrouter",
                bearer_auth_material(config.api_key.clone().unwrap_or_default()),
            ));
            if let Some(base_url) =
                resolve_provider_transcription_base_url(ProviderKind::Openrouter, args)
            {
                config.base_url = base_url;
            }
            AdditionalTranscriptionBackendConfig::openrouter(config)
        }
        ProviderKind::Anthropic | ProviderKind::Google | ProviderKind::Xai => {
            bail!(
                "{} does not support dedicated transcription backends",
                request
                    .provider
                    .api_key_env_key()
                    .trim_end_matches("_API_KEY")
                    .to_lowercase()
            )
        }
    };
    Ok(vec![backend])
}

/// Resolves the primary Anthropic route for legacy `serve` configuration.
pub(crate) async fn resolve_anthropic_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<AnthropicProviderConfig> {
    let selected = resolve_anthropic_auth_source(args, "anthropic-default")?;
    let model = resolve_model(args.provider, args.model.as_deref());

    match selected {
        ResolvedAnthropicAuthSource::ApiKey => {
            let api_key = resolve_api_key(ProviderKind::Anthropic, args.api_key.as_deref())
                .ok_or_else(|| anyhow!("missing ANTHROPIC_API_KEY for kheish-daemon serve"))?;
            Ok(configure_anthropic_provider(
                args,
                AnthropicProviderConfig::with_request_auth_provider(
                    model,
                    inline_request_provider(
                        manager.clone(),
                        "anthropic",
                        header_auth_material("x-api-key", api_key),
                    ),
                ),
            ))
        }
        ResolvedAnthropicAuthSource::ClaudeCode => {
            ensure_anthropic_claude_code_slot(
                &manager,
                AuthSlotId::new("anthropic-default"),
                resolve_anthropic_credentials_path(args),
            )
            .await?;
            Ok(configure_anthropic_provider(
                args,
                AnthropicProviderConfig::with_request_auth_provider(
                    model,
                    brokered_request_provider(
                        manager.clone(),
                        AuthSlotId::new("anthropic-default"),
                        "anthropic",
                    ),
                ),
            ))
        }
    }
}

/// Resolves the primary OpenAI route for legacy `serve` configuration.
#[cfg(test)]
pub(crate) async fn resolve_openai_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<OpenAiProviderConfig> {
    let selected = resolve_openai_auth_source(args, "openai-default")?;
    resolve_openai_provider_for_source(args, manager, selected).await
}

async fn resolve_openai_provider_for_source(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
    selected: ResolvedOpenAiAuthSource,
) -> Result<OpenAiProviderConfig> {
    let base_url = args.openai_base_url.clone();
    let organization = args
        .openai_organization
        .clone()
        .or_else(|| std::env::var("OPENAI_ORGANIZATION").ok());
    let project = args
        .openai_project
        .clone()
        .or_else(|| std::env::var("OPENAI_PROJECT").ok());
    let model = resolve_model(args.provider, args.model.as_deref());

    match selected {
        ResolvedOpenAiAuthSource::ApiKey => {
            let api_key = resolve_api_key(ProviderKind::Openai, args.api_key.as_deref())
                .ok_or_else(|| anyhow!("missing OPENAI_API_KEY for kheish-daemon serve"))?;
            Ok(configure_openai_provider(
                OpenAiProviderConfig::with_request_auth_provider(
                    model,
                    inline_request_provider(
                        manager.clone(),
                        "openai",
                        bearer_auth_material(api_key),
                    ),
                ),
                base_url,
                organization,
                project,
            ))
        }
        ResolvedOpenAiAuthSource::Codex => {
            ensure_openai_codex_slot(
                &manager,
                AuthSlotId::new("openai-default"),
                resolve_openai_auth_file_path(args),
                organization.clone(),
                project.clone(),
            )
            .await?;
            Ok(configure_openai_provider(
                OpenAiProviderConfig::with_request_auth_provider(
                    model,
                    brokered_request_provider(
                        manager.clone(),
                        AuthSlotId::new("openai-default"),
                        "openai",
                    ),
                ),
                base_url,
                organization,
                project,
            ))
        }
    }
}

/// Resolves the primary OpenRouter route for legacy `serve` configuration.
pub(crate) async fn resolve_openrouter_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<OpenRouterProviderConfig> {
    let model = resolve_model(args.provider, args.model.as_deref());
    let api_key = resolve_api_key(ProviderKind::Openrouter, args.api_key.as_deref())
        .ok_or_else(|| anyhow!("missing OPENROUTER_API_KEY for kheish-daemon serve"))?;
    let provider = configure_openrouter_provider(
        OpenRouterProviderConfig::with_request_auth_provider(
            model,
            inline_request_provider(manager, "openrouter", bearer_auth_material(api_key)),
        ),
        args.openrouter_base_url.clone(),
    );
    enrich_openrouter_provider_with_discovery(provider).await
}

/// Resolves the primary Google route for legacy `serve` configuration.
pub(crate) async fn resolve_google_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<GoogleProviderConfig> {
    let model = resolve_model(args.provider, args.model.as_deref());
    let api_key = resolve_google_route_api_key(args, true).ok_or_else(|| {
        anyhow!("missing GOOGLE_API_KEY or GEMINI_API_KEY for kheish-daemon serve")
    })?;
    let mut provider = GoogleProviderConfig::with_request_auth_provider(
        model,
        inline_request_provider(
            manager,
            "google",
            header_auth_material("x-goog-api-key", api_key),
        ),
    );
    if let Some(base_url) = resolve_google_base_url(args) {
        provider.base_url = base_url;
    }
    Ok(provider)
}

/// Resolves the primary xAI route for legacy `serve` configuration.
pub(crate) async fn resolve_xai_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<XAiProviderConfig> {
    let model = resolve_model(args.provider, args.model.as_deref());
    let api_key = resolve_api_key(ProviderKind::Xai, args.api_key.as_deref())
        .ok_or_else(|| anyhow!("missing XAI_API_KEY for kheish-daemon serve"))?;
    let mut provider = XAiProviderConfig::with_request_auth_provider(
        model,
        inline_request_provider(manager, "xai", bearer_auth_material(api_key)),
    );
    if let Some(base_url) = args
        .xai_base_url
        .clone()
        .or_else(|| std::env::var("XAI_BASE_URL").ok())
    {
        provider.base_url = base_url;
    }
    Ok(provider)
}

/// Loads model routes from a routes file and applies the default-route ordering.
pub(crate) async fn resolve_route_inventory_from_file(
    args: &ServeArgs,
    path: &Path,
    manager: Arc<AuthManager>,
) -> Result<Vec<ConfiguredModelRoute>> {
    let config = RoutesFileConfig::from_path(path)?;
    let default_route = args
        .default_route
        .clone()
        .unwrap_or(config.effective_default_route()?);
    let mut routes = Vec::with_capacity(config.routes.len());
    for (route_id, entry) in config.routes {
        routes.push(resolve_configured_route(args, &route_id, entry, manager.clone()).await?);
    }
    move_default_route_first(&mut routes, &default_route)?;
    Ok(routes)
}

/// Resolves a route model with explicit, shared, and provider-specific overrides.
pub(crate) fn resolve_model_with(
    provider: ProviderKind,
    explicit: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> String {
    if provider == ProviderKind::Google {
        return explicit
            .map(str::to_string)
            .or_else(|| lookup("KHEISH_MODEL"))
            .or_else(|| lookup("KHEISH_GOOGLE_MODEL"))
            .or_else(|| lookup("GOOGLE_MODEL"))
            .or_else(|| lookup("GEMINI_MODEL"))
            .unwrap_or_else(|| provider.default_model().to_string());
    }
    explicit
        .map(str::to_string)
        .or_else(|| lookup("KHEISH_MODEL"))
        .or_else(|| lookup(provider.model_env_key()))
        .unwrap_or_else(|| provider.default_model().to_string())
}

/// Resolves a provider API key with explicit, shared, and provider-specific overrides.
pub(crate) fn resolve_api_key_with(
    provider: ProviderKind,
    explicit: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if provider == ProviderKind::Google {
        return explicit
            .map(str::to_string)
            .or_else(|| lookup("KHEISH_API_KEY"))
            .or_else(|| lookup("KHEISH_GOOGLE_API_KEY"))
            .or_else(|| lookup("GOOGLE_API_KEY"))
            .or_else(|| lookup("GEMINI_API_KEY"));
    }
    explicit
        .map(str::to_string)
        .or_else(|| lookup("KHEISH_API_KEY"))
        .or_else(|| lookup(provider.api_key_env_key()))
}

/// Resolves the effective OpenAI auth source for one daemon instance.
pub(crate) fn resolve_openai_auth_source(
    args: &ServeArgs,
    slot_name: &str,
) -> Result<ResolvedOpenAiAuthSource> {
    let source = args.openai_auth_source.unwrap_or(OpenAiAuthSourceArg::Auto);
    let api_key_present = resolve_api_key(ProviderKind::Openai, args.api_key.as_deref()).is_some();
    let slot_exists = auth_slot_exists(&args.state_root, slot_name)?;
    let codex_auth_available =
        resolve_openai_auth_file_path(args).is_some_and(|path| path.exists());
    if let Some(path) = args.openai_auth_file.as_ref() {
        let needs_account_auth = matches!(source, OpenAiAuthSourceArg::Codex)
            || matches!(source, OpenAiAuthSourceArg::Auto) && !api_key_present;
        if needs_account_auth && !slot_exists && !path.exists() {
            bail!(
                "no Codex auth file was available at {} for OpenAI account auth",
                path.display()
            );
        }
    }

    select_openai_auth_source(source, api_key_present, slot_exists, codex_auth_available)
}

/// Resolves the effective Anthropic auth source for one daemon instance.
pub(crate) fn resolve_anthropic_auth_source(
    args: &ServeArgs,
    slot_name: &str,
) -> Result<ResolvedAnthropicAuthSource> {
    let source = args
        .anthropic_auth_source
        .unwrap_or(AnthropicAuthSourceArg::Auto);
    let api_key_present =
        resolve_api_key(ProviderKind::Anthropic, args.api_key.as_deref()).is_some();
    let slot_exists = auth_slot_exists(&args.state_root, slot_name)?;
    let claude_code_auth_available =
        resolve_anthropic_credentials_path(args).is_some_and(|path| path.exists());
    if let Some(path) = args.anthropic_credentials_file.as_ref() {
        let needs_account_auth = matches!(source, AnthropicAuthSourceArg::ClaudeCode)
            || matches!(source, AnthropicAuthSourceArg::Auto) && !api_key_present;
        if needs_account_auth && !slot_exists && !path.exists() {
            bail!(
                "no Claude Code credentials file was available at {} for Anthropic account auth",
                path.display()
            );
        }
    }
    select_anthropic_auth_source(
        source,
        api_key_present,
        slot_exists,
        claude_code_auth_available,
    )
}

/// Chooses the OpenAI auth source from the available inputs.
pub(crate) fn select_openai_auth_source(
    source: OpenAiAuthSourceArg,
    api_key_present: bool,
    slot_exists: bool,
    codex_auth_available: bool,
) -> Result<ResolvedOpenAiAuthSource> {
    match source {
        OpenAiAuthSourceArg::ApiKey => {
            if api_key_present {
                Ok(ResolvedOpenAiAuthSource::ApiKey)
            } else {
                Err(anyhow!("missing OPENAI_API_KEY for kheish-daemon serve"))
            }
        }
        OpenAiAuthSourceArg::Codex => {
            if slot_exists || codex_auth_available {
                Ok(ResolvedOpenAiAuthSource::Codex)
            } else {
                Err(anyhow!(
                    "no persisted OpenAI account auth slot was found and no Codex auth.json was available"
                ))
            }
        }
        OpenAiAuthSourceArg::Auto => {
            if api_key_present {
                Ok(ResolvedOpenAiAuthSource::ApiKey)
            } else if slot_exists || codex_auth_available {
                Ok(ResolvedOpenAiAuthSource::Codex)
            } else {
                Err(anyhow!(
                    "missing OPENAI_API_KEY for kheish-daemon serve and no Codex account auth was available"
                ))
            }
        }
    }
}

/// Chooses the Anthropic auth source from the available inputs.
pub(crate) fn select_anthropic_auth_source(
    source: AnthropicAuthSourceArg,
    api_key_present: bool,
    slot_exists: bool,
    claude_code_auth_available: bool,
) -> Result<ResolvedAnthropicAuthSource> {
    match source {
        AnthropicAuthSourceArg::ApiKey => {
            if api_key_present {
                Ok(ResolvedAnthropicAuthSource::ApiKey)
            } else {
                Err(anyhow!("missing ANTHROPIC_API_KEY for kheish-daemon serve"))
            }
        }
        AnthropicAuthSourceArg::ClaudeCode => {
            if slot_exists || claude_code_auth_available {
                Ok(ResolvedAnthropicAuthSource::ClaudeCode)
            } else {
                Err(anyhow!(
                    "no persisted Anthropic account auth slot was found and no Claude Code credentials were available"
                ))
            }
        }
        AnthropicAuthSourceArg::Auto => {
            if api_key_present {
                Ok(ResolvedAnthropicAuthSource::ApiKey)
            } else if slot_exists || claude_code_auth_available {
                Ok(ResolvedAnthropicAuthSource::ClaudeCode)
            } else {
                Err(anyhow!(
                    "missing ANTHROPIC_API_KEY for kheish-daemon serve and no Claude Code account auth was available"
                ))
            }
        }
    }
}

/// Resolves a route-specific value from inline config or an environment variable.
pub(crate) fn resolve_route_value(inline: Option<&str>, env_name: Option<&str>) -> Option<String> {
    inline
        .map(str::to_string)
        .or_else(|| env_name.and_then(|name| std::env::var(name).ok()))
}

/// Resolves the API key for one configured route.
pub(crate) fn resolve_route_api_key(route_id: &str, entry: &RouteFileEntry) -> Option<String> {
    if let Some(api_key) = entry.api_key.clone() {
        return Some(api_key);
    }
    if let Some(env_name) = entry.api_key_env.as_deref() {
        return std::env::var(env_name).ok();
    }
    if !route_uses_driver_defaults(route_id, entry.driver) {
        return None;
    }
    match entry.driver {
        RouteFileDriver::Anthropic => resolve_api_key(ProviderKind::Anthropic, None),
        RouteFileDriver::Google => {
            resolve_api_key_with(ProviderKind::Google, None, |key| std::env::var(key).ok())
        }
        RouteFileDriver::Openai => resolve_api_key(ProviderKind::Openai, None),
        RouteFileDriver::Openrouter => resolve_api_key(ProviderKind::Openrouter, None),
        RouteFileDriver::Xai => resolve_api_key(ProviderKind::Xai, None),
    }
}

/// Resolves the effective OpenAI auth source for one configured route.
pub(crate) fn resolve_route_openai_auth_source(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
) -> Result<ResolvedOpenAiAuthSource> {
    let api_key_present = resolve_route_api_key(route_id, entry).is_some();
    let slot_exists = auth_slot_exists(&args.state_root, &route_auth_slot_id(route_id).0)?;
    let auth_available =
        resolve_route_openai_auth_file_path(route_id, entry).is_some_and(|path| path.exists());
    let source = match entry.openai_auth_source {
        Some(RouteFileOpenAiAuthSource::ApiKey) => OpenAiAuthSourceArg::ApiKey,
        Some(RouteFileOpenAiAuthSource::Codex) => OpenAiAuthSourceArg::Codex,
        None if route_uses_driver_defaults(route_id, RouteFileDriver::Openai) => {
            OpenAiAuthSourceArg::Auto
        }
        None => OpenAiAuthSourceArg::ApiKey,
    };
    if matches!(source, OpenAiAuthSourceArg::ApiKey) && !api_key_present {
        bail!(
            "route `{route_id}` is missing an OpenAI-compatible API key; set api_key/api_key_env or choose openai_auth_source = \"codex\""
        );
    }
    select_openai_auth_source(source, api_key_present, slot_exists, auth_available)
}

/// Resolves the effective Anthropic auth source for one configured route.
pub(crate) fn resolve_route_anthropic_auth_source(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
) -> Result<ResolvedAnthropicAuthSource> {
    let api_key_present = resolve_route_api_key(route_id, entry).is_some();
    let slot_exists = auth_slot_exists(&args.state_root, &route_auth_slot_id(route_id).0)?;
    let auth_available =
        resolve_route_anthropic_credentials_path(route_id, entry).is_some_and(|path| path.exists());
    let source = match entry.anthropic_auth_source {
        Some(RouteFileAnthropicAuthSource::ApiKey) => AnthropicAuthSourceArg::ApiKey,
        Some(RouteFileAnthropicAuthSource::ClaudeCode) => AnthropicAuthSourceArg::ClaudeCode,
        None if route_uses_driver_defaults(route_id, RouteFileDriver::Anthropic) => {
            AnthropicAuthSourceArg::Auto
        }
        None => AnthropicAuthSourceArg::ApiKey,
    };
    if matches!(source, AnthropicAuthSourceArg::ApiKey) && !api_key_present {
        bail!(
            "route `{route_id}` is missing an Anthropic API key; set api_key/api_key_env or choose anthropic_auth_source = \"claude_code\""
        );
    }
    select_anthropic_auth_source(source, api_key_present, slot_exists, auth_available)
}

/// Returns the persisted auth slot identifier used for one configured route.
pub(crate) fn route_auth_slot_id(route_id: &str) -> AuthSlotId {
    AuthSlotId::new(format!("route.{route_id}"))
}

/// Ensures that an OpenAI Codex auth slot exists before serving requests.
pub(crate) async fn ensure_openai_codex_slot(
    manager: &Arc<AuthManager>,
    slot_id: AuthSlotId,
    codex_auth_path: Option<PathBuf>,
    organization: Option<String>,
    project: Option<String>,
) -> Result<()> {
    if manager.has_slot(&slot_id).await {
        return Ok(());
    }
    let codex_auth_path = codex_auth_path
        .ok_or_else(|| anyhow!("no Codex auth.json was available for OpenAI account auth"))?;
    if !codex_auth_path.exists() {
        bail!(
            "no Codex auth file was available at {} for OpenAI account auth",
            codex_auth_path.display()
        );
    }
    ensure_openai_codex_slot_from_path(manager, slot_id, codex_auth_path, organization, project)
        .await
}

/// Imports an OpenAI Codex auth record from a specific auth file path.
pub(crate) async fn ensure_openai_codex_slot_from_path(
    manager: &Arc<AuthManager>,
    slot_id: AuthSlotId,
    codex_auth_path: PathBuf,
    organization: Option<String>,
    project: Option<String>,
) -> Result<()> {
    if manager.has_slot(&slot_id).await {
        return Ok(());
    }
    let issuer = std::env::var("KHEISH_OPENAI_AUTH_ISSUER")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_OPENAI_AUTH_ISSUER.to_string());
    let client_id = std::env::var("KHEISH_OPENAI_AUTH_CLIENT_ID")
        .unwrap_or_else(|_| kheish_auth::DEFAULT_CODEX_CLIENT_ID.to_string());
    let api_base_url = std::env::var("KHEISH_OPENAI_CODEX_API_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_OPENAI_CODEX_API_BASE_URL.to_string());
    let record = OpenAiAuthBackend::import_codex_record_with_overrides(
        slot_id,
        codex_auth_path,
        issuer,
        client_id,
        api_base_url,
        organization,
        project,
    )?;
    ensure_secret_manager_master_key_configured()?;
    manager.put_record(record).await?;
    Ok(())
}

/// Ensures that a Claude Code auth slot exists before serving Anthropic requests.
pub(crate) async fn ensure_anthropic_claude_code_slot(
    manager: &Arc<AuthManager>,
    slot_id: AuthSlotId,
    credentials_path: Option<PathBuf>,
) -> Result<()> {
    if manager.has_slot(&slot_id).await {
        return Ok(());
    }
    let credentials_path = credentials_path.ok_or_else(|| {
        anyhow!("no Claude Code credentials were available for Anthropic account auth")
    })?;
    if !credentials_path.exists() {
        bail!(
            "no Claude Code credentials file was available at {} for Anthropic account auth",
            credentials_path.display()
        );
    }
    ensure_anthropic_claude_code_slot_from_path(manager, slot_id, credentials_path).await
}

/// Imports an Anthropic Claude Code auth record from a specific credentials path.
pub(crate) async fn ensure_anthropic_claude_code_slot_from_path(
    manager: &Arc<AuthManager>,
    slot_id: AuthSlotId,
    credentials_path: PathBuf,
) -> Result<()> {
    if manager.has_slot(&slot_id).await {
        return Ok(());
    }
    let token_url = std::env::var("KHEISH_ANTHROPIC_OAUTH_TOKEN_URL")
        .unwrap_or_else(|_| DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL.to_string());
    let client_id = std::env::var("KHEISH_ANTHROPIC_OAUTH_CLIENT_ID")
        .unwrap_or_else(|_| DEFAULT_CLAUDE_CODE_CLIENT_ID.to_string());
    let record = AnthropicAuthBackend::import_claude_code_record_with_overrides(
        slot_id,
        credentials_path,
        token_url,
        client_id,
    )?;
    ensure_secret_manager_master_key_configured()?;
    manager.put_record(record).await?;
    Ok(())
}

async fn resolve_legacy_route_inventory(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Vec<ConfiguredModelRoute>> {
    let mut routes = Vec::new();
    match args.provider {
        ProviderKind::Anthropic => {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                resolve_anthropic_provider(args, manager.clone()).await?,
            )));
        }
        ProviderKind::Google => {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::Google(
                resolve_google_provider(args, manager.clone()).await?,
            )));
        }
        ProviderKind::Openai => {
            let selected = resolve_openai_auth_source(args, "openai-default")?;
            let route = ConfiguredModelRoute::legacy(ModelRouteConfig::OpenAi(
                resolve_openai_provider_for_source(args, manager.clone(), selected).await?,
            ));
            routes.push(if matches!(selected, ResolvedOpenAiAuthSource::Codex) {
                let capabilities = route.capabilities().clone();
                route.with_capabilities(openai_account_auth_media_disabled_capabilities(
                    capabilities,
                ))
            } else {
                route
            });
        }
        ProviderKind::Openrouter => {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::OpenRouter(
                resolve_openrouter_provider(args, manager.clone()).await?,
            )));
        }
        ProviderKind::Xai => {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::XAi(
                resolve_xai_provider(args, manager.clone()).await?,
            )));
        }
    }

    if args.provider != ProviderKind::Anthropic {
        if let Some(provider) = resolve_anthropic_fallback_provider(args, manager.clone()).await? {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::Anthropic(
                provider,
            )));
        }
    }
    if args.provider != ProviderKind::Google {
        if let Some(provider) = resolve_google_fallback_provider(args, manager.clone()).await? {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::Google(
                provider,
            )));
        }
    }
    if args.provider != ProviderKind::Openai {
        if let Some((provider, selected)) =
            resolve_openai_fallback_provider(args, manager.clone()).await?
        {
            let route = ConfiguredModelRoute::legacy(ModelRouteConfig::OpenAi(provider));
            routes.push(if matches!(selected, ResolvedOpenAiAuthSource::Codex) {
                let capabilities = route.capabilities().clone();
                route.with_capabilities(openai_account_auth_media_disabled_capabilities(
                    capabilities,
                ))
            } else {
                route
            });
        }
    }
    if args.provider != ProviderKind::Xai {
        if let Some(provider) = resolve_xai_fallback_provider(args, manager.clone()).await? {
            routes.push(ConfiguredModelRoute::legacy(ModelRouteConfig::XAi(
                provider,
            )));
        }
    }
    if let Some(default_route) = args.default_route.as_deref() {
        move_default_route_first(&mut routes, default_route)?;
    }
    Ok(routes)
}

async fn resolve_configured_route(
    args: &ServeArgs,
    route_id: &str,
    entry: RouteFileEntry,
    manager: Arc<AuthManager>,
) -> Result<ConfiguredModelRoute> {
    let route = match entry.driver {
        RouteFileDriver::Anthropic => {
            resolve_configured_anthropic_route(args, route_id, &entry, manager).await?
        }
        RouteFileDriver::Google => {
            resolve_configured_google_route(args, route_id, &entry, manager).await?
        }
        RouteFileDriver::Openai => {
            resolve_configured_openai_route(args, route_id, &entry, manager).await?
        }
        RouteFileDriver::Openrouter => {
            resolve_configured_openrouter_route(args, route_id, &entry, manager).await?
        }
        RouteFileDriver::Xai => {
            resolve_configured_xai_route(args, route_id, &entry, manager).await?
        }
    };
    if openai_account_auth_media_is_disabled(&route)
        && explicit_openai_media_capability_enabled(&entry)
    {
        bail!(
            "route `{route_id}` uses OpenAI Codex account auth, which supports Responses text/tool calls only; disable image/audio/transcription capability overrides or use OpenAI API-key auth for media"
        );
    }
    if route_file_entry_has_capability_override(&entry) {
        let capabilities = entry.resolved_capabilities(route.capabilities().clone());
        Ok(route.with_capabilities(capabilities))
    } else {
        Ok(route)
    }
}

fn openai_account_auth_media_disabled_capabilities(
    mut capabilities: RouteCapabilities,
) -> RouteCapabilities {
    capabilities.image_generation = false;
    capabilities.image_edit = false;
    capabilities.audio_generation = false;
    capabilities.transcription = false;
    capabilities
}

fn openai_account_auth_media_is_disabled(route: &ConfiguredModelRoute) -> bool {
    route.provider_name() == "openai"
        && !route.capabilities().image_generation
        && !route.capabilities().image_edit
        && !route.capabilities().audio_generation
        && !route.capabilities().transcription
}

fn explicit_openai_media_capability_enabled(entry: &RouteFileEntry) -> bool {
    entry.image_generation == Some(true)
        || entry.image_edit == Some(true)
        || entry.audio_generation == Some(true)
        || entry.transcription == Some(true)
}

fn route_file_entry_has_capability_override(entry: &RouteFileEntry) -> bool {
    entry.multimodal_input.is_some()
        || entry.native_web_search.is_some()
        || entry.image_generation.is_some()
        || entry.image_edit.is_some()
        || entry.audio_generation.is_some()
        || entry.transcription.is_some()
}

/// Resolves one configured OpenAI route from a routes-file entry.
pub(crate) async fn resolve_configured_openai_route(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
    manager: Arc<AuthManager>,
) -> Result<ConfiguredModelRoute> {
    let organization = resolve_route_value(
        entry.organization.as_deref(),
        entry.organization_env.as_deref(),
    );
    let project = resolve_route_value(entry.project.as_deref(), entry.project_env.as_deref());
    let mut auth_ref = None;
    let mut codex_account_auth = false;
    let mut provider = if let Some(slot_id) =
        resolve_route_auth_slot(&manager, route_id, entry, AuthProvider::OpenAi).await?
    {
        let status = manager
            .status(&slot_id)
            .await?
            .ok_or_else(|| anyhow!("route `{route_id}` references missing auth_ref `{slot_id}`"))?;
        codex_account_auth = status.mode == AuthMode::OAuthAccount;
        auth_ref = Some(slot_id.0.clone());
        OpenAiProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            brokered_request_provider(manager.clone(), slot_id, route_id),
        )
    } else {
        let selected = resolve_route_openai_auth_source(args, route_id, entry)?;
        match selected {
            ResolvedOpenAiAuthSource::ApiKey => {
                let api_key = resolve_route_api_key(route_id, entry).ok_or_else(|| {
                    anyhow!("route `{route_id}` is missing an OpenAI-compatible API key")
                })?;
                OpenAiProviderConfig::with_request_auth_provider(
                    entry.default_model.clone(),
                    inline_request_provider(
                        manager.clone(),
                        route_id,
                        bearer_auth_material(api_key),
                    ),
                )
            }
            ResolvedOpenAiAuthSource::Codex => {
                codex_account_auth = true;
                let slot_id = route_auth_slot_id(route_id);
                ensure_openai_codex_slot(
                    &manager,
                    slot_id.clone(),
                    resolve_route_openai_auth_file_path(route_id, entry),
                    organization.clone(),
                    project.clone(),
                )
                .await?;
                auth_ref = Some(slot_id.0.clone());
                OpenAiProviderConfig::with_request_auth_provider(
                    entry.default_model.clone(),
                    brokered_request_provider(manager.clone(), slot_id, route_id),
                )
            }
        }
    };
    provider = configure_openai_provider(
        provider,
        entry
            .base_url
            .clone()
            .or_else(|| args.openai_base_url.clone()),
        organization,
        project,
    );
    let route = ConfiguredModelRoute::new(route_id.to_string(), ModelRouteConfig::OpenAi(provider))
        .with_auth_ref(auth_ref)
        .with_model_support(entry.model_support);
    Ok(if codex_account_auth {
        let capabilities = route.capabilities().clone();
        route.with_capabilities(openai_account_auth_media_disabled_capabilities(
            capabilities,
        ))
    } else {
        route
    })
}

/// Resolves one configured OpenRouter route from a routes-file entry.
pub(crate) async fn resolve_configured_openrouter_route(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
    manager: Arc<AuthManager>,
) -> Result<ConfiguredModelRoute> {
    let mut auth_ref = None;
    let mut provider = if let Some(slot_id) =
        resolve_route_auth_slot(&manager, route_id, entry, AuthProvider::OpenRouter).await?
    {
        auth_ref = Some(slot_id.0.clone());
        OpenRouterProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            brokered_request_provider(manager.clone(), slot_id, route_id),
        )
    } else {
        let api_key = resolve_route_api_key(route_id, entry)
            .ok_or_else(|| anyhow!("route `{route_id}` is missing an OpenRouter API key"))?;
        OpenRouterProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            inline_request_provider(manager.clone(), route_id, bearer_auth_material(api_key)),
        )
    };
    provider = configure_openrouter_provider(
        provider,
        entry
            .base_url
            .clone()
            .or_else(|| args.openrouter_base_url.clone()),
    );
    provider = enrich_openrouter_provider_with_discovery(provider).await?;
    Ok(
        ConfiguredModelRoute::new(route_id.to_string(), ModelRouteConfig::OpenRouter(provider))
            .with_auth_ref(auth_ref)
            .with_model_support(openrouter_model_support(entry.model_support)),
    )
}

fn openrouter_model_support(configured: ModelSupportPolicy) -> ModelSupportPolicy {
    match configured {
        ModelSupportPolicy::Family => ModelSupportPolicy::Any,
        ModelSupportPolicy::Any => ModelSupportPolicy::Any,
    }
}

/// Resolves one configured Anthropic route from a routes-file entry.
pub(crate) async fn resolve_configured_anthropic_route(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
    manager: Arc<AuthManager>,
) -> Result<ConfiguredModelRoute> {
    let mut auth_ref = None;
    let mut provider = if let Some(slot_id) =
        resolve_route_auth_slot(&manager, route_id, entry, AuthProvider::Anthropic).await?
    {
        auth_ref = Some(slot_id.0.clone());
        AnthropicProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            brokered_request_provider(manager.clone(), slot_id, route_id),
        )
    } else {
        let selected = resolve_route_anthropic_auth_source(args, route_id, entry)?;
        match selected {
            ResolvedAnthropicAuthSource::ApiKey => {
                let api_key = resolve_route_api_key(route_id, entry)
                    .ok_or_else(|| anyhow!("route `{route_id}` is missing an Anthropic API key"))?;
                AnthropicProviderConfig::with_request_auth_provider(
                    entry.default_model.clone(),
                    inline_request_provider(
                        manager.clone(),
                        route_id,
                        header_auth_material("x-api-key", api_key),
                    ),
                )
            }
            ResolvedAnthropicAuthSource::ClaudeCode => {
                let slot_id = route_auth_slot_id(route_id);
                ensure_anthropic_claude_code_slot(
                    &manager,
                    slot_id.clone(),
                    resolve_route_anthropic_credentials_path(route_id, entry),
                )
                .await?;
                auth_ref = Some(slot_id.0.clone());
                AnthropicProviderConfig::with_request_auth_provider(
                    entry.default_model.clone(),
                    brokered_request_provider(manager.clone(), slot_id, route_id),
                )
            }
        }
    };
    if let Some(base_url) = entry
        .base_url
        .clone()
        .or_else(|| args.anthropic_base_url.clone())
    {
        provider.base_url = base_url;
    }
    if let Some(version) = entry.anthropic_version.clone() {
        provider.anthropic_version = version;
    }
    provider.beta_headers = entry.anthropic_beta_headers.clone();
    Ok(
        ConfiguredModelRoute::new(route_id.to_string(), ModelRouteConfig::Anthropic(provider))
            .with_auth_ref(auth_ref)
            .with_model_support(entry.model_support),
    )
}

async fn resolve_configured_google_route(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
    manager: Arc<AuthManager>,
) -> Result<ConfiguredModelRoute> {
    let mut auth_ref = None;
    let mut provider = if let Some(slot_id) =
        resolve_route_auth_slot(&manager, route_id, entry, AuthProvider::Google).await?
    {
        auth_ref = Some(slot_id.0.clone());
        GoogleProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            brokered_request_provider(manager.clone(), slot_id, route_id),
        )
    } else {
        let api_key = resolve_route_api_key(route_id, entry)
            .ok_or_else(|| anyhow!("route `{route_id}` is missing a Google API key"))?;
        GoogleProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            inline_request_provider(
                manager.clone(),
                route_id,
                header_auth_material("x-goog-api-key", api_key),
            ),
        )
    };
    if let Some(base_url) = entry
        .base_url
        .clone()
        .or_else(|| resolve_google_base_url(args))
    {
        provider.base_url = base_url;
    }
    Ok(
        ConfiguredModelRoute::new(route_id.to_string(), ModelRouteConfig::Google(provider))
            .with_auth_ref(auth_ref)
            .with_model_support(entry.model_support),
    )
}

async fn resolve_configured_xai_route(
    args: &ServeArgs,
    route_id: &str,
    entry: &RouteFileEntry,
    manager: Arc<AuthManager>,
) -> Result<ConfiguredModelRoute> {
    let mut auth_ref = None;
    let mut provider = if let Some(slot_id) =
        resolve_route_auth_slot(&manager, route_id, entry, AuthProvider::XAi).await?
    {
        auth_ref = Some(slot_id.0.clone());
        XAiProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            brokered_request_provider(manager.clone(), slot_id, route_id),
        )
    } else {
        let api_key = resolve_route_api_key(route_id, entry)
            .ok_or_else(|| anyhow!("route `{route_id}` is missing an xAI API key"))?;
        XAiProviderConfig::with_request_auth_provider(
            entry.default_model.clone(),
            inline_request_provider(manager.clone(), route_id, bearer_auth_material(api_key)),
        )
    };
    if let Some(base_url) = entry.base_url.clone().or_else(|| {
        args.xai_base_url
            .clone()
            .or_else(|| std::env::var("XAI_BASE_URL").ok())
    }) {
        provider.base_url = base_url;
    }
    Ok(
        ConfiguredModelRoute::new(route_id.to_string(), ModelRouteConfig::XAi(provider))
            .with_auth_ref(auth_ref)
            .with_model_support(entry.model_support),
    )
}

/// Moves the configured default route to the front of the route inventory.
pub(crate) fn move_default_route_first(
    routes: &mut Vec<ConfiguredModelRoute>,
    default_route: &str,
) -> Result<()> {
    let Some(index) = routes
        .iter()
        .position(|route| route.route_id() == default_route)
    else {
        bail!("default route `{default_route}` is not configured");
    };
    if index > 0 {
        let route = routes.remove(index);
        routes.insert(0, route);
    }
    Ok(())
}

async fn resolve_route_auth_slot(
    manager: &AuthManager,
    route_id: &str,
    entry: &RouteFileEntry,
    expected_provider: AuthProvider,
) -> Result<Option<AuthSlotId>> {
    let Some(auth_ref) = entry.auth_ref.as_deref() else {
        return Ok(None);
    };
    let slot_id = AuthSlotId::new(auth_ref.to_string());
    let status = manager
        .status(&slot_id)
        .await?
        .ok_or_else(|| anyhow!("route `{route_id}` references missing auth_ref `{auth_ref}`"))?;
    if status.provider != expected_provider {
        bail!(
            "route `{route_id}` auth_ref `{auth_ref}` targets provider `{}` but route driver requires `{expected_provider}`",
            status.provider
        );
    }
    Ok(Some(slot_id))
}

pub(crate) fn route_uses_driver_defaults(route_id: &str, driver: RouteFileDriver) -> bool {
    match driver {
        RouteFileDriver::Anthropic => route_id == "anthropic",
        RouteFileDriver::Google => route_id == "google",
        RouteFileDriver::Openai => route_id == "openai",
        RouteFileDriver::Openrouter => route_id == "openrouter",
        RouteFileDriver::Xai => route_id == "xai",
    }
}

fn resolve_route_openai_auth_file_path(route_id: &str, entry: &RouteFileEntry) -> Option<PathBuf> {
    entry.openai_auth_file.clone().or_else(|| {
        (route_uses_driver_defaults(route_id, RouteFileDriver::Openai)
            || matches!(
                entry.openai_auth_source,
                Some(RouteFileOpenAiAuthSource::Codex)
            ))
        .then(default_codex_auth_path)
        .flatten()
    })
}

fn resolve_route_anthropic_credentials_path(
    route_id: &str,
    entry: &RouteFileEntry,
) -> Option<PathBuf> {
    entry.anthropic_credentials_file.clone().or_else(|| {
        (route_uses_driver_defaults(route_id, RouteFileDriver::Anthropic)
            || matches!(
                entry.anthropic_auth_source,
                Some(RouteFileAnthropicAuthSource::ClaudeCode)
            ))
        .then(default_claude_code_credentials_path)
        .flatten()
    })
}

async fn resolve_google_fallback_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Option<GoogleProviderConfig>> {
    let Some(api_key) = resolve_google_route_api_key(args, false) else {
        return Ok(None);
    };
    let model = resolve_google_route_model(None, false);
    let mut provider = GoogleProviderConfig::with_request_auth_provider(
        model,
        inline_request_provider(
            manager,
            "google",
            header_auth_material("x-goog-api-key", api_key),
        ),
    );
    if let Some(base_url) = resolve_google_base_url(args) {
        provider.base_url = base_url;
    }
    Ok(Some(provider))
}

async fn resolve_openai_fallback_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Option<(OpenAiProviderConfig, ResolvedOpenAiAuthSource)>> {
    let Ok(selected) = resolve_openai_auth_source(args, "openai-fallback") else {
        return Ok(None);
    };
    let base_url = args.openai_base_url.clone();
    let organization = args
        .openai_organization
        .clone()
        .or_else(|| std::env::var("OPENAI_ORGANIZATION").ok());
    let project = args
        .openai_project
        .clone()
        .or_else(|| std::env::var("OPENAI_PROJECT").ok());
    let model = std::env::var(ProviderKind::Openai.model_env_key())
        .unwrap_or_else(|_| ProviderKind::Openai.default_model().to_string());

    match selected {
        ResolvedOpenAiAuthSource::ApiKey => {
            let Some(api_key) = std::env::var(ProviderKind::Openai.api_key_env_key()).ok() else {
                return Ok(None);
            };
            Ok(Some((
                configure_openai_provider(
                    OpenAiProviderConfig::with_request_auth_provider(
                        model,
                        inline_request_provider(
                            manager.clone(),
                            "openai",
                            bearer_auth_material(api_key),
                        ),
                    ),
                    base_url,
                    organization,
                    project,
                ),
                selected,
            )))
        }
        ResolvedOpenAiAuthSource::Codex => {
            ensure_openai_codex_slot(
                &manager,
                AuthSlotId::new("openai-fallback"),
                resolve_openai_auth_file_path(args),
                organization.clone(),
                project.clone(),
            )
            .await?;
            Ok(Some((
                configure_openai_provider(
                    OpenAiProviderConfig::with_request_auth_provider(
                        model,
                        brokered_request_provider(
                            manager.clone(),
                            AuthSlotId::new("openai-fallback"),
                            "openai",
                        ),
                    ),
                    base_url,
                    organization,
                    project,
                ),
                selected,
            )))
        }
    }
}

async fn resolve_xai_fallback_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Option<XAiProviderConfig>> {
    let Some(api_key) = std::env::var(ProviderKind::Xai.api_key_env_key()).ok() else {
        return Ok(None);
    };
    let model = std::env::var(ProviderKind::Xai.model_env_key())
        .unwrap_or_else(|_| ProviderKind::Xai.default_model().to_string());
    let mut provider = XAiProviderConfig::with_request_auth_provider(
        model,
        inline_request_provider(manager, "xai", bearer_auth_material(api_key)),
    );
    if let Some(base_url) = args
        .xai_base_url
        .clone()
        .or_else(|| std::env::var("XAI_BASE_URL").ok())
    {
        provider.base_url = base_url;
    }
    Ok(Some(provider))
}

async fn resolve_anthropic_fallback_provider(
    args: &ServeArgs,
    manager: Arc<AuthManager>,
) -> Result<Option<AnthropicProviderConfig>> {
    let Ok(selected) = resolve_anthropic_auth_source(args, "anthropic-fallback") else {
        return Ok(None);
    };
    let model = std::env::var(ProviderKind::Anthropic.model_env_key())
        .unwrap_or_else(|_| ProviderKind::Anthropic.default_model().to_string());
    match selected {
        ResolvedAnthropicAuthSource::ApiKey => {
            let Some(api_key) = std::env::var(ProviderKind::Anthropic.api_key_env_key()).ok()
            else {
                return Ok(None);
            };
            Ok(Some(configure_anthropic_provider(
                args,
                AnthropicProviderConfig::with_request_auth_provider(
                    model,
                    inline_request_provider(
                        manager.clone(),
                        "anthropic",
                        header_auth_material("x-api-key", api_key),
                    ),
                ),
            )))
        }
        ResolvedAnthropicAuthSource::ClaudeCode => {
            ensure_anthropic_claude_code_slot(
                &manager,
                AuthSlotId::new("anthropic-fallback"),
                resolve_anthropic_credentials_path(args),
            )
            .await?;
            Ok(Some(configure_anthropic_provider(
                args,
                AnthropicProviderConfig::with_request_auth_provider(
                    model,
                    brokered_request_provider(
                        manager.clone(),
                        AuthSlotId::new("anthropic-fallback"),
                        "anthropic",
                    ),
                ),
            )))
        }
    }
}

fn auth_slot_exists(state_root: &Path, slot_name: &str) -> Result<bool> {
    Ok(FileAuthStore::new(global_auth_store_path(state_root))
        .load()?
        .slots
        .contains_key(slot_name))
}

/// Resolves the OpenAI Codex auth file path for one daemon instance.
pub(crate) fn resolve_openai_auth_file_path(args: &ServeArgs) -> Option<PathBuf> {
    args.openai_auth_file
        .clone()
        .or_else(default_codex_auth_path)
}

/// Resolves the Anthropic Claude Code credentials path for one daemon instance.
pub(crate) fn resolve_anthropic_credentials_path(args: &ServeArgs) -> Option<PathBuf> {
    args.anthropic_credentials_file
        .clone()
        .or_else(default_claude_code_credentials_path)
}

fn configure_anthropic_provider(
    args: &ServeArgs,
    mut provider: AnthropicProviderConfig,
) -> AnthropicProviderConfig {
    if let Some(base_url) = args.anthropic_base_url.clone() {
        provider.base_url = base_url;
    }
    if let Some(version) = args.anthropic_version.clone() {
        provider.anthropic_version = version;
    }
    provider.beta_headers = args.anthropic_beta_headers.clone();
    provider
}

fn configure_openai_provider(
    mut provider: OpenAiProviderConfig,
    base_url: Option<String>,
    organization: Option<String>,
    project: Option<String>,
) -> OpenAiProviderConfig {
    if let Some(base_url) = base_url {
        provider.base_url = base_url;
    }
    provider.organization = organization;
    provider.project = project;
    provider
}

fn configure_openrouter_provider(
    mut provider: OpenRouterProviderConfig,
    base_url: Option<String>,
) -> OpenRouterProviderConfig {
    if let Some(base_url) = base_url {
        provider.base_url = base_url;
    }
    provider
}

async fn enrich_openrouter_provider_with_discovery(
    provider: OpenRouterProviderConfig,
) -> Result<OpenRouterProviderConfig> {
    match openrouter_discovery_mode(&provider.base_url) {
        OpenRouterDiscoveryMode::Disabled => Ok(provider),
        OpenRouterDiscoveryMode::BestEffort => {
            match fetch_openrouter_model_capabilities(&provider).await {
                Ok(capabilities) if !capabilities.is_empty() => {
                    Ok(provider.with_model_capabilities(capabilities))
                }
                Ok(_) => Ok(provider),
                Err(_) => Ok(provider),
            }
        }
        OpenRouterDiscoveryMode::Required => {
            let capabilities = fetch_openrouter_model_capabilities(&provider)
                .await
                .map_err(|error| anyhow!("OpenRouter model discovery failed: {}", error.message))?;
            if capabilities.is_empty() {
                bail!("OpenRouter model discovery returned no models");
            }
            Ok(provider.with_model_capabilities(capabilities))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenRouterDiscoveryMode {
    Disabled,
    BestEffort,
    Required,
}

fn openrouter_discovery_mode(base_url: &str) -> OpenRouterDiscoveryMode {
    match std::env::var("KHEISH_OPENROUTER_MODEL_DISCOVERY")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "0" | "false" | "off" | "disabled" => OpenRouterDiscoveryMode::Disabled,
        "required" | "require" | "strict" => OpenRouterDiscoveryMode::Required,
        "1" | "true" | "on" | "enabled" | "best_effort" | "best-effort" => {
            OpenRouterDiscoveryMode::BestEffort
        }
        _ if base_url.contains("openrouter.ai") => OpenRouterDiscoveryMode::BestEffort,
        _ => OpenRouterDiscoveryMode::Disabled,
    }
}

fn resolve_model(provider: ProviderKind, explicit: Option<&str>) -> String {
    resolve_model_with(provider, explicit, |key| std::env::var(key).ok())
}

fn resolve_api_key(provider: ProviderKind, explicit: Option<&str>) -> Option<String> {
    resolve_api_key_with(provider, explicit, |key| std::env::var(key).ok())
}

fn resolve_google_route_model(explicit: Option<&str>, include_generic: bool) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| {
            include_generic
                .then(|| std::env::var("KHEISH_MODEL").ok())
                .flatten()
        })
        .or_else(|| std::env::var("KHEISH_GOOGLE_MODEL").ok())
        .or_else(|| std::env::var("GOOGLE_MODEL").ok())
        .or_else(|| std::env::var("GEMINI_MODEL").ok())
        .unwrap_or_else(|| ProviderKind::Google.default_model().to_string())
}

fn resolve_google_route_api_key(args: &ServeArgs, include_generic: bool) -> Option<String> {
    args.google_api_key
        .clone()
        .or_else(|| include_generic.then(|| args.api_key.clone()).flatten())
        .or_else(|| {
            include_generic
                .then(|| std::env::var("KHEISH_API_KEY").ok())
                .flatten()
        })
        .or_else(|| std::env::var("KHEISH_GOOGLE_API_KEY").ok())
        .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
}

fn resolve_google_base_url(args: &ServeArgs) -> Option<String> {
    args.google_base_url
        .clone()
        .or_else(|| std::env::var("GOOGLE_BASE_URL").ok())
        .or_else(|| std::env::var("GEMINI_BASE_URL").ok())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestedImageBackend {
    provider: ProviderKind,
    model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestedTranscriptionBackend {
    provider: ProviderKind,
    model: Option<String>,
}

fn resolve_image_provider_override(args: &ServeArgs) -> Result<Option<ProviderKind>> {
    match args.image_provider {
        Some(provider) => Ok(Some(provider)),
        None => {
            let Some(raw) = std::env::var("KHEISH_IMAGE_PROVIDER").ok() else {
                return Ok(None);
            };
            <ProviderKind as ValueEnum>::from_str(&raw, true)
                .map(Some)
                .map_err(|_| anyhow!("invalid KHEISH_IMAGE_PROVIDER value {raw:?}"))
        }
    }
}

fn resolve_generic_image_model_override(args: &ServeArgs) -> Option<String> {
    args.image_model
        .clone()
        .or_else(|| std::env::var("KHEISH_IMAGE_MODEL").ok())
}

fn resolve_legacy_google_image_model_override(args: &ServeArgs) -> Option<String> {
    args.google_image_model
        .clone()
        .or_else(|| std::env::var("GOOGLE_IMAGE_MODEL").ok())
        .or_else(|| std::env::var("GEMINI_IMAGE_MODEL").ok())
}

fn resolve_requested_image_backend(args: &ServeArgs) -> Result<Option<RequestedImageBackend>> {
    let provider_override = resolve_image_provider_override(args)?;
    let generic_model_override = resolve_generic_image_model_override(args);
    let legacy_google_model_override = resolve_legacy_google_image_model_override(args);

    match (
        provider_override,
        generic_model_override,
        legacy_google_model_override,
    ) {
        (None, None, None) => Ok(None),
        (Some(provider), model, legacy_google_model) => Ok(Some(RequestedImageBackend {
            provider,
            model: model.or_else(|| {
                (provider == ProviderKind::Google)
                    .then_some(legacy_google_model)
                    .flatten()
            }),
        })),
        (None, Some(model), _) => Ok(Some(RequestedImageBackend {
            provider: args.provider,
            model: Some(model),
        })),
        (None, None, Some(model)) => Ok(Some(RequestedImageBackend {
            provider: ProviderKind::Google,
            model: Some(model),
        })),
    }
}

fn resolve_provider_image_api_key(
    provider: ProviderKind,
    args: &ServeArgs,
    include_generic: bool,
) -> Option<String> {
    if let Some(api_key) = args
        .image_api_key
        .clone()
        .or_else(|| std::env::var("KHEISH_IMAGE_API_KEY").ok())
    {
        return Some(api_key);
    }
    match provider {
        ProviderKind::Google => resolve_google_route_api_key(args, include_generic),
        other => {
            let explicit = include_generic.then_some(args.api_key.as_deref()).flatten();
            if include_generic {
                resolve_api_key(other, explicit)
            } else {
                std::env::var(other.api_key_env_key()).ok()
            }
        }
    }
}

fn resolve_provider_image_base_url(provider: ProviderKind, args: &ServeArgs) -> Option<String> {
    match provider {
        ProviderKind::Anthropic => args.anthropic_base_url.clone(),
        ProviderKind::Google => resolve_google_base_url(args),
        ProviderKind::Openai => args.openai_base_url.clone(),
        ProviderKind::Openrouter => args.openrouter_base_url.clone(),
        ProviderKind::Xai => args
            .xai_base_url
            .clone()
            .or_else(|| std::env::var("XAI_BASE_URL").ok()),
    }
}

fn resolve_transcription_provider_override(args: &ServeArgs) -> Result<Option<ProviderKind>> {
    match args.transcription_provider {
        Some(provider) => Ok(Some(provider)),
        None => {
            let Some(raw) = std::env::var("KHEISH_TRANSCRIPTION_PROVIDER").ok() else {
                return Ok(None);
            };
            <ProviderKind as ValueEnum>::from_str(&raw, true)
                .map(Some)
                .map_err(|_| anyhow!("invalid KHEISH_TRANSCRIPTION_PROVIDER value {raw:?}"))
        }
    }
}

fn resolve_requested_transcription_backend(
    args: &ServeArgs,
) -> Result<Option<RequestedTranscriptionBackend>> {
    let provider_override = resolve_transcription_provider_override(args)?;
    let model_override = args
        .transcription_model
        .clone()
        .or_else(|| std::env::var("KHEISH_TRANSCRIPTION_MODEL").ok());
    match (provider_override, model_override) {
        (None, None) => Ok(None),
        (Some(provider), model) => Ok(Some(RequestedTranscriptionBackend { provider, model })),
        (None, Some(model)) => Ok(Some(RequestedTranscriptionBackend {
            provider: args.provider,
            model: Some(model),
        })),
    }
}

fn resolve_provider_transcription_api_key(
    provider: ProviderKind,
    args: &ServeArgs,
    include_generic: bool,
) -> Option<String> {
    if let Some(api_key) = args
        .transcription_api_key
        .clone()
        .or_else(|| std::env::var("KHEISH_TRANSCRIPTION_API_KEY").ok())
    {
        return Some(api_key);
    }
    match provider {
        ProviderKind::Google => resolve_google_route_api_key(args, include_generic),
        other => {
            let explicit = include_generic.then_some(args.api_key.as_deref()).flatten();
            if include_generic {
                resolve_api_key(other, explicit)
            } else {
                std::env::var(other.api_key_env_key()).ok()
            }
        }
    }
}

fn resolve_provider_transcription_base_url(
    provider: ProviderKind,
    args: &ServeArgs,
) -> Option<String> {
    args.transcription_base_url
        .clone()
        .or_else(|| match provider {
            ProviderKind::Anthropic => args.anthropic_base_url.clone(),
            ProviderKind::Google => resolve_google_base_url(args),
            ProviderKind::Openai => args.openai_base_url.clone(),
            ProviderKind::Openrouter => args.openrouter_base_url.clone(),
            ProviderKind::Xai => args
                .xai_base_url
                .clone()
                .or_else(|| std::env::var("XAI_BASE_URL").ok()),
        })
}

impl ProviderKind {
    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => DEFAULT_ANTHROPIC_MODEL,
            Self::Google => DEFAULT_GOOGLE_MODEL,
            Self::Openai => DEFAULT_OPENAI_MODEL,
            Self::Openrouter => DEFAULT_OPENROUTER_MODEL,
            Self::Xai => DEFAULT_XAI_MODEL,
        }
    }

    fn model_env_key(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_MODEL",
            Self::Google => "GOOGLE_MODEL",
            Self::Openai => "OPENAI_MODEL",
            Self::Openrouter => "OPENROUTER_MODEL",
            Self::Xai => "XAI_MODEL",
        }
    }

    fn api_key_env_key(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Google => "GOOGLE_API_KEY",
            Self::Openai => "OPENAI_API_KEY",
            Self::Openrouter => "OPENROUTER_API_KEY",
            Self::Xai => "XAI_API_KEY",
        }
    }
}
