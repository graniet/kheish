//! Route file parsing for daemon text model routes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kheish_daemon::{ModelSupportPolicy, ROUTE_CAPABILITY_MATRIX_VERSION, RouteCapabilities};
use reqwest::Url;
use serde::{Deserialize, Serialize};

/// Supported route drivers that can back one configured daemon route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteFileDriver {
    Anthropic,
    Google,
    Openai,
    Openrouter,
    Xai,
}

impl RouteFileDriver {
    /// Returns the stable route driver identifier used in route diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Openai => "openai",
            Self::Openrouter => "openrouter",
            Self::Xai => "xai",
        }
    }

    /// Returns base daemon-visible capabilities for this driver before explicit file overrides.
    pub fn supported_capabilities(self) -> RouteCapabilities {
        match self {
            Self::Anthropic => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: false,
                image_edit: false,
                audio_generation: false,
                transcription: false,
            },
            Self::Google => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: false,
                image_generation: true,
                image_edit: true,
                audio_generation: false,
                transcription: false,
            },
            Self::Openai => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
            Self::Openrouter => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: false,
                image_generation: true,
                image_edit: true,
                audio_generation: true,
                transcription: true,
            },
            Self::Xai => RouteCapabilities {
                matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
                multimodal_input: true,
                native_web_search: true,
                image_generation: true,
                image_edit: true,
                audio_generation: false,
                transcription: false,
            },
        }
    }
}

/// OpenAI route authentication sources supported in a routes file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteFileOpenAiAuthSource {
    ApiKey,
    Codex,
}

/// Anthropic route authentication sources supported in a routes file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteFileAnthropicAuthSource {
    ApiKey,
    ClaudeCode,
}

/// One named route entry loaded from TOML.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteFileEntry {
    pub driver: RouteFileDriver,
    pub default_model: String,
    #[serde(default)]
    pub model_support: ModelSupportPolicy,
    #[serde(default)]
    pub auth_ref: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub organization_env: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub project_env: Option<String>,
    #[serde(default)]
    pub anthropic_version: Option<String>,
    #[serde(default)]
    pub anthropic_beta_headers: Vec<String>,
    #[serde(default)]
    pub openai_auth_source: Option<RouteFileOpenAiAuthSource>,
    #[serde(default)]
    pub openai_auth_file: Option<PathBuf>,
    #[serde(default)]
    pub anthropic_auth_source: Option<RouteFileAnthropicAuthSource>,
    #[serde(default)]
    pub anthropic_credentials_file: Option<PathBuf>,
    #[serde(default)]
    pub multimodal_input: Option<bool>,
    #[serde(default)]
    pub native_web_search: Option<bool>,
    #[serde(default)]
    pub image_generation: Option<bool>,
    #[serde(default)]
    pub image_edit: Option<bool>,
    #[serde(default)]
    pub audio_generation: Option<bool>,
    #[serde(default)]
    pub transcription: Option<bool>,
}

impl RouteFileEntry {
    /// Applies optional capability overrides on top of the driver defaults.
    pub fn resolved_capabilities(&self, base: RouteCapabilities) -> RouteCapabilities {
        RouteCapabilities {
            matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
            multimodal_input: self.multimodal_input.unwrap_or(base.multimodal_input),
            native_web_search: self.native_web_search.unwrap_or(base.native_web_search),
            image_generation: self.image_generation.unwrap_or(base.image_generation),
            image_edit: self.image_edit.unwrap_or(base.image_edit),
            audio_generation: self.audio_generation.unwrap_or(base.audio_generation),
            transcription: self.transcription.unwrap_or(base.transcription),
        }
    }

    fn validate(&self, route_id: &str) -> Result<()> {
        if self.default_model.trim().is_empty() {
            bail!("route `{route_id}` requires a non-empty default_model");
        }
        if let Some(base_url) = self.base_url.as_deref() {
            validate_route_base_url(route_id, base_url)?;
        }
        if self.api_key.is_some() && self.api_key_env.is_some() {
            bail!("route `{route_id}` cannot combine api_key and api_key_env");
        }
        if let Some(api_key) = self.api_key.as_deref()
            && api_key.trim().is_empty()
        {
            bail!("route `{route_id}` api_key must not be empty");
        }
        if let Some(api_key_env) = self.api_key_env.as_deref()
            && api_key_env.trim().is_empty()
        {
            bail!("route `{route_id}` api_key_env must not be empty");
        }
        if let Some(auth_ref) = self.auth_ref.as_deref() {
            if auth_ref.trim().is_empty() {
                bail!("route `{route_id}` auth_ref must not be empty");
            }
            if auth_ref.trim() != auth_ref {
                bail!("route `{route_id}` auth_ref must not have leading or trailing whitespace");
            }
        }
        if self.auth_ref.is_some() && (self.api_key.is_some() || self.api_key_env.is_some()) {
            bail!("route `{route_id}` cannot combine auth_ref with api_key or api_key_env");
        }
        if self.organization.is_some() && self.organization_env.is_some() {
            bail!("route `{route_id}` cannot combine organization and organization_env");
        }
        if self.project.is_some() && self.project_env.is_some() {
            bail!("route `{route_id}` cannot combine project and project_env");
        }
        if self.auth_ref.is_some()
            && (self.openai_auth_source.is_some() || self.openai_auth_file.is_some())
        {
            bail!("route `{route_id}` cannot combine auth_ref with openai_auth_* settings");
        }
        if self.auth_ref.is_some()
            && (self.anthropic_auth_source.is_some() || self.anthropic_credentials_file.is_some())
        {
            bail!("route `{route_id}` cannot combine auth_ref with anthropic_auth_* settings");
        }
        if self.openai_auth_source.is_some() && self.driver != RouteFileDriver::Openai {
            bail!("route `{route_id}` only openai routes may set openai_auth_source");
        }
        if self.openai_auth_file.is_some() && self.driver != RouteFileDriver::Openai {
            bail!("route `{route_id}` only openai routes may set openai_auth_file");
        }
        if self.anthropic_auth_source.is_some() && self.driver != RouteFileDriver::Anthropic {
            bail!("route `{route_id}` only anthropic routes may set anthropic_auth_source");
        }
        if self.anthropic_credentials_file.is_some() && self.driver != RouteFileDriver::Anthropic {
            bail!("route `{route_id}` only anthropic routes may set anthropic_credentials_file");
        }
        Ok(())
    }
}

/// The complete TOML route file consumed by `kheish-daemon serve`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutesFileConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub default_route: Option<String>,
    #[serde(default)]
    pub routes: BTreeMap<String, RouteFileEntry>,
}

impl RoutesFileConfig {
    /// Parses and validates one routes TOML file.
    pub fn from_toml_str(raw: &str) -> Result<Self> {
        let parsed =
            toml::from_str::<Self>(raw).map_err(|error| sanitized_routes_toml_error(raw, error))?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Loads and validates one routes TOML file from disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_toml_str(&raw)
    }

    /// Resolves the default route identifier if one can be chosen safely.
    pub fn effective_default_route(&self) -> Result<String> {
        if let Some(default_route) = self.default_route.as_deref() {
            return Ok(default_route.to_string());
        }
        match self.routes.len() {
            0 => bail!("routes file must define at least one route"),
            1 => Ok(self
                .routes
                .keys()
                .next()
                .expect("one route exists when len == 1")
                .to_string()),
            _ => bail!("routes file must set default_route when multiple routes are configured"),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported routes file version {}; expected 1",
                self.version
            );
        }
        if self.routes.is_empty() {
            bail!("routes file must define at least one route");
        }
        for (route_id, entry) in &self.routes {
            validate_route_id(route_id)?;
            entry.validate(route_id)?;
        }
        if let Some(default_route) = self.default_route.as_deref()
            && !self.routes.contains_key(default_route)
        {
            bail!("default_route `{default_route}` is not defined under [routes]");
        }
        Ok(())
    }
}

fn default_version() -> u32 {
    1
}

fn validate_route_id(route_id: &str) -> Result<()> {
    let trimmed = route_id.trim();
    if trimmed.is_empty() {
        bail!("route identifiers must not be empty");
    }
    if trimmed != route_id {
        bail!("route identifiers must not have leading or trailing whitespace");
    }
    if trimmed.contains('/') {
        bail!("route `{trimmed}` must not contain '/'");
    }
    if trimmed.chars().any(char::is_whitespace) {
        bail!("route `{trimmed}` must not contain whitespace");
    }
    Ok(())
}

fn sanitized_routes_toml_error(raw: &str, error: toml::de::Error) -> anyhow::Error {
    let location = error
        .span()
        .map(|span| line_column_for_offset(raw, span.start))
        .map(|(line, column)| format!(" at line {line}, column {column}"))
        .unwrap_or_default();
    anyhow::anyhow!("failed to parse routes TOML{location}: invalid syntax or route schema")
}

fn line_column_for_offset(raw: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut column = 1usize;
    for (index, byte) in raw.bytes().enumerate() {
        if index >= offset {
            break;
        }
        if byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn validate_route_base_url(route_id: &str, value: &str) -> Result<()> {
    let url = Url::parse(value)
        .with_context(|| format!("route `{route_id}` base_url is not a valid absolute URL"))?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            bail!("route `{route_id}` base_url must use http:// or https://, got {scheme}")
        }
    }
    if url.host_str().is_none() {
        bail!("route `{route_id}` base_url is missing a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("route `{route_id}` base_url must not include userinfo");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("route `{route_id}` base_url must not include query or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn parses_routes_file_and_resolves_default_route() -> Result<()> {
        let config = RoutesFileConfig::from_toml_str(
            r#"
version = 1
default_route = "openrouter"

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
model_support = "any"
native_web_search = false

[routes.proxy]
driver = "openrouter"
default_model = "anthropic/claude-sonnet-4"

[routes.anthropic]
driver = "anthropic"
default_model = "claude-opus-4-6"
"#,
        )?;
        assert_eq!(config.effective_default_route()?, "openrouter");
        let route = config.routes.get("openrouter").expect("route");
        assert_eq!(route.driver, RouteFileDriver::Openai);
        assert_eq!(route.model_support, ModelSupportPolicy::Any);
        let proxy = config.routes.get("proxy").expect("proxy route");
        assert_eq!(proxy.driver, RouteFileDriver::Openrouter);
        assert!(RouteFileDriver::Xai.supported_capabilities().image_edit);
        let capabilities = route.resolved_capabilities(RouteCapabilities {
            matrix_version: ROUTE_CAPABILITY_MATRIX_VERSION,
            multimodal_input: true,
            native_web_search: true,
            image_generation: false,
            image_edit: false,
            audio_generation: false,
            transcription: true,
        });
        assert!(!capabilities.native_web_search);
        assert!(capabilities.multimodal_input);
        assert!(capabilities.transcription);
        Ok(())
    }

    #[test]
    fn loads_routes_file_from_disk() -> Result<()> {
        let temp = TempDir::new()?;
        let path = temp.path().join("routes.toml");
        std::fs::write(
            &path,
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
"#,
        )?;
        let config = RoutesFileConfig::from_path(&path)?;
        assert_eq!(config.effective_default_route()?, "openai");
        Ok(())
    }

    #[test]
    fn rejects_invalid_route_identifier() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes."openrouter/main"]
driver = "openai"
default_model = "gpt-5.4"
"#,
        )
        .expect_err("invalid route id should fail");
        assert!(error.to_string().contains("must not contain '/'"));
    }

    #[test]
    fn rejects_route_identifier_with_edge_whitespace() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes." openai "]
driver = "openai"
default_model = "gpt-5.4"
"#,
        )
        .expect_err("route id edge whitespace should fail");
        assert!(
            error
                .to_string()
                .contains("must not have leading or trailing whitespace")
        );
    }

    #[test]
    fn rejects_auth_ref_with_edge_whitespace() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
auth_ref = " openai.prod "
"#,
        )
        .expect_err("auth_ref edge whitespace should fail");
        assert!(
            error
                .to_string()
                .contains("auth_ref must not have leading or trailing whitespace")
        );
    }

    #[test]
    fn rejects_conflicting_api_key_sources() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
api_key = "inline-key"
api_key_env = "OPENROUTER_API_KEY"
"#,
        )
        .expect_err("conflicting api key sources should fail");
        assert!(
            error
                .to_string()
                .contains("cannot combine api_key and api_key_env")
        );
    }

    #[test]
    fn rejects_empty_api_key_sources() {
        let inline_error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openrouter]
driver = "openrouter"
default_model = "openai/gpt-5.4-mini"
api_key = "   "
"#,
        )
        .expect_err("empty inline api key should fail");
        assert!(
            inline_error
                .to_string()
                .contains("api_key must not be empty")
        );

        let env_error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openrouter]
driver = "openrouter"
default_model = "openai/gpt-5.4-mini"
api_key_env = "   "
"#,
        )
        .expect_err("empty api_key_env should fail");
        assert!(
            env_error
                .to_string()
                .contains("api_key_env must not be empty")
        );
    }

    #[test]
    fn parse_errors_do_not_echo_toml_source_context() {
        let error = RoutesFileConfig::from_toml_str(
            "version = 1\n\
             \n\
             [routes.openai]\n\
             driver = \"openai\"\n\
             default_model = \"gpt-5.4\"\n\
             api_key = \"sk-doctor-secret\n",
        )
        .expect_err("malformed TOML should fail");
        let rendered = error.to_string();
        assert!(rendered.contains("failed to parse routes TOML"));
        assert!(rendered.contains("line"));
        assert!(
            !rendered.contains("sk-doctor-secret") && !rendered.contains("api_key ="),
            "route TOML parse errors must not leak source snippets: {rendered}"
        );
    }

    #[test]
    fn parse_errors_do_not_echo_invalid_enum_values() {
        let invalid_driver = format!("{}{}", "sk-", "doctor-secret-driver");
        let error = RoutesFileConfig::from_toml_str(&format!(
            r#"
version = 1

[routes.openai]
driver = "{invalid_driver}"
default_model = "gpt-5.4"
"#,
        ))
        .expect_err("invalid enum value should fail");
        let rendered = error.to_string();
        assert!(rendered.contains("invalid syntax or route schema"));
        assert!(
            !rendered.contains(&invalid_driver),
            "route TOML parse errors must not leak invalid raw values: {rendered}"
        );
    }

    #[test]
    fn rejects_invalid_route_base_url() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
base_url = "openai.example/v1/responses"
"#,
        )
        .expect_err("relative base_url should fail route-file validation");
        assert!(
            error
                .to_string()
                .contains("base_url is not a valid absolute URL")
        );
    }

    #[test]
    fn rejects_route_base_url_userinfo_without_leaking_value() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
base_url = "https://user:secret@api.openai.example/v1/responses"
"#,
        )
        .expect_err("base_url userinfo should fail route-file validation");
        let rendered = error.to_string();
        assert!(rendered.contains("base_url must not include userinfo"));
        assert!(
            !rendered.contains("secret"),
            "base_url diagnostics should not echo userinfo secrets: {rendered}"
        );
    }

    #[test]
    fn rejects_route_base_url_query_or_fragment() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
base_url = "https://api.openai.example/v1/responses?token=secret"
"#,
        )
        .expect_err("base_url query should fail route-file validation");
        let rendered = error.to_string();
        assert!(rendered.contains("base_url must not include query or fragment"));
        assert!(
            !rendered.contains("token=secret"),
            "base_url diagnostics should not echo query secrets: {rendered}"
        );
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1
default_route = "openai"
routez = "typo"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
"#,
        )
        .expect_err("unknown top-level fields should fail strict route-file parsing");
        assert!(error.to_string().contains("invalid syntax or route schema"));
    }

    #[test]
    fn rejects_unknown_route_fields() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
api_key_enb = "OPENAI_API_KEY"
"#,
        )
        .expect_err("unknown route fields should fail strict route-file parsing");
        assert!(error.to_string().contains("invalid syntax or route schema"));
    }

    #[test]
    fn rejects_auth_ref_combined_with_inline_auth_sources() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
auth_ref = "openrouter.primary"
api_key_env = "OPENROUTER_API_KEY"
"#,
        )
        .expect_err("auth_ref should conflict with inline auth sources");
        assert!(
            error
                .to_string()
                .contains("cannot combine auth_ref with api_key or api_key_env")
        );
    }

    #[test]
    fn rejects_auth_ref_combined_with_driver_specific_auth_settings() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
auth_ref = "openai.prod"
openai_auth_source = "codex"
"#,
        )
        .expect_err("auth_ref should conflict with openai auth settings");
        assert!(
            error
                .to_string()
                .contains("cannot combine auth_ref with openai_auth_* settings")
        );
    }

    #[test]
    fn rejects_driver_specific_auth_fields_on_the_wrong_driver() {
        let error = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.google]
driver = "google"
default_model = "gemini-2.5-flash"
openai_auth_source = "codex"
"#,
        )
        .expect_err("driver-specific auth fields should fail on the wrong driver");
        assert!(
            error
                .to_string()
                .contains("only openai routes may set openai_auth_source")
        );
    }

    #[test]
    fn effective_default_route_requires_explicit_default_for_multiple_routes() -> Result<()> {
        let config = RoutesFileConfig::from_toml_str(
            r#"
version = 1

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"

[routes.anthropic]
driver = "anthropic"
default_model = "claude-opus-4-6"
"#,
        )?;
        let error = config
            .effective_default_route()
            .expect_err("multi-route files require a default route");
        assert!(
            error
                .to_string()
                .contains("must set default_route when multiple routes are configured")
        );
        Ok(())
    }
}
