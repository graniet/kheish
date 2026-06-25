use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AuthSlotId, McpOAuthAccountRecordInput, now_ms};

/// Result of MCP protected-resource and authorization-server discovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthDiscovery {
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
}

/// One generated OAuth authorization request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthAuthorizationRequest {
    pub authorization_url: String,
    pub redirect_uri: String,
    pub state: String,
    pub code_verifier: String,
    pub code_challenge: String,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for McpOAuthAuthorizationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthAuthorizationRequest")
            .field("authorization_url", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &"<redacted>")
            .field("code_verifier", &"<redacted>")
            .field("code_challenge", &self.code_challenge)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Token response stored by Kheish after a completed OAuth authorization code flow.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthTokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for McpOAuthTokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthTokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Deserialize)]
struct DynamicClientRegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// Discovers OAuth 2.1 metadata for one HTTP MCP resource URL.
pub async fn discover_mcp_oauth(
    client: &Client,
    resource_url: &str,
    allow_http_for_loopback: bool,
) -> Result<McpOAuthDiscovery> {
    let resource = canonical_resource_url(resource_url, allow_http_for_loopback)?;
    let protected =
        discover_protected_resource_metadata(client, &resource, allow_http_for_loopback).await?;
    let issuer = protected
        .authorization_servers
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("MCP protected resource metadata has no authorization_servers"))?;
    let issuer = secure_oauth_url(&issuer, "issuer", allow_http_for_loopback, true)?;
    let metadata =
        discover_authorization_server_metadata(client, &issuer, allow_http_for_loopback).await?;
    if !metadata.code_challenge_methods_supported.is_empty()
        && !metadata
            .code_challenge_methods_supported
            .iter()
            .any(|method| method.eq_ignore_ascii_case("S256"))
    {
        bail!("authorization server does not advertise PKCE S256 support");
    }
    let discovered_resource = protected
        .resource
        .map(|resource| canonical_resource_url(&resource, allow_http_for_loopback))
        .transpose()?
        .unwrap_or_else(|| resource.clone());
    anyhow::ensure!(
        discovered_resource == resource,
        "MCP protected resource metadata resource does not match requested resource"
    );
    Ok(McpOAuthDiscovery {
        resource: discovered_resource,
        issuer: secure_oauth_url(&metadata.issuer, "issuer", allow_http_for_loopback, true)?,
        authorization_endpoint: secure_oauth_url(
            &metadata.authorization_endpoint,
            "authorization_endpoint",
            allow_http_for_loopback,
            false,
        )?,
        token_endpoint: secure_oauth_url(
            &metadata.token_endpoint,
            "token_endpoint",
            allow_http_for_loopback,
            false,
        )?,
        registration_endpoint: metadata
            .registration_endpoint
            .as_deref()
            .map(|endpoint| {
                secure_oauth_url(
                    endpoint,
                    "registration_endpoint",
                    allow_http_for_loopback,
                    false,
                )
            })
            .transpose()?,
        scopes_supported: normalize_scopes(if protected.scopes_supported.is_empty() {
            metadata.scopes_supported
        } else {
            protected.scopes_supported
        }),
    })
}

/// Registers a public OAuth client through DCR when the MCP authorization server advertises it.
pub async fn dynamic_register_mcp_oauth_client(
    client: &Client,
    discovery: &McpOAuthDiscovery,
    redirect_uri: &str,
) -> Result<(String, Option<String>)> {
    let endpoint = discovery
        .registration_endpoint
        .as_deref()
        .ok_or_else(|| anyhow!("authorization server does not advertise dynamic registration"))?;
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({
            "client_name": "Kheish",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .context("failed to dynamically register MCP OAuth client")?;
    if !response.status().is_success() {
        let status = response.status();
        let _ = response.text().await;
        bail!(
            "MCP OAuth dynamic client registration failed with status {status}: response body redacted"
        );
    }
    let registered = response
        .json::<DynamicClientRegistrationResponse>()
        .await
        .context("failed to decode dynamic client registration response")?;
    Ok((registered.client_id, registered.client_secret))
}

/// Builds a PKCE S256 authorization URL.
pub fn build_mcp_oauth_authorization_request(
    discovery: &McpOAuthDiscovery,
    client_id: &str,
    redirect_uri: &str,
    requested_scopes: &[String],
) -> Result<McpOAuthAuthorizationRequest> {
    let state = random_urlsafe(32);
    let code_verifier = random_urlsafe(32);
    let code_challenge = pkce_s256_challenge(&code_verifier);
    let scopes = if requested_scopes.is_empty() {
        discovery.scopes_supported.clone()
    } else {
        normalize_scopes(requested_scopes.to_vec())
    };
    let mut url = Url::parse(&discovery.authorization_endpoint)
        .context("invalid OAuth authorization_endpoint")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("state", &state);
        query.append_pair("code_challenge", &code_challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("resource", &discovery.resource);
        if !scopes.is_empty() {
            query.append_pair("scope", &scopes.join(" "));
        }
    }
    Ok(McpOAuthAuthorizationRequest {
        authorization_url: url.to_string(),
        redirect_uri: redirect_uri.to_string(),
        state,
        code_verifier,
        code_challenge,
        scopes,
    })
}

/// Exchanges an authorization code for tokens using PKCE.
pub async fn exchange_mcp_oauth_code(
    client: &Client,
    discovery: &McpOAuthDiscovery,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
    scopes: &[String],
) -> Result<McpOAuthTokenSet> {
    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("code_verifier".to_string(), code_verifier.to_string()),
        ("resource".to_string(), discovery.resource.clone()),
    ];
    if let Some(client_secret) = client_secret {
        form.push(("client_secret".to_string(), client_secret.to_string()));
    }
    if !scopes.is_empty() {
        form.push(("scope".to_string(), scopes.join(" ")));
    }
    let response = client
        .post(&discovery.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await
        .context("failed to exchange MCP OAuth authorization code")?;
    if !response.status().is_success() {
        let status = response.status();
        let _ = response.text().await;
        bail!(
            "MCP OAuth authorization code exchange failed with status {status}: response body redacted"
        );
    }
    let token = response
        .json::<TokenResponse>()
        .await
        .context("failed to decode MCP OAuth token response")?;
    if let Some(token_type) = token.token_type.as_deref() {
        anyhow::ensure!(
            token_type.eq_ignore_ascii_case("bearer"),
            "MCP OAuth token endpoint returned unsupported token_type `{token_type}`"
        );
    }
    let returned_scopes = token
        .scope
        .as_deref()
        .map(normalize_scope_string)
        .unwrap_or_else(|| scopes.to_vec());
    ensure_no_scope_escalation(scopes, &returned_scopes)?;
    Ok(McpOAuthTokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_ms: token
            .expires_in
            .map(|expires_in| now_ms().saturating_add(expires_in.saturating_mul(1_000))),
        scopes: normalize_scopes(returned_scopes),
    })
}

/// Builds the auth-store input for one completed MCP OAuth login.
pub fn mcp_oauth_account_input(
    slot_id: AuthSlotId,
    server_name: String,
    discovery: &McpOAuthDiscovery,
    client_id: String,
    client_secret: Option<String>,
    token: McpOAuthTokenSet,
) -> McpOAuthAccountRecordInput {
    McpOAuthAccountRecordInput {
        slot_id,
        server_name,
        resource: discovery.resource.clone(),
        issuer: discovery.issuer.clone(),
        authorization_endpoint: discovery.authorization_endpoint.clone(),
        token_endpoint: discovery.token_endpoint.clone(),
        client_id,
        client_secret,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_ms: token.expires_at_ms,
        scopes: token.scopes,
    }
}

pub fn canonical_resource_url(resource_url: &str, allow_http_for_loopback: bool) -> Result<String> {
    let mut parsed = Url::parse(resource_url).context("invalid MCP resource URL")?;
    anyhow::ensure!(
        parsed.fragment().is_none(),
        "MCP OAuth resource URL must not contain a fragment"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "MCP OAuth resource URL must not contain userinfo"
    );
    for (key, _) in parsed.query_pairs() {
        anyhow::ensure!(
            !oauth_url_query_key_is_sensitive(&key),
            "MCP OAuth resource URL must not contain sensitive query parameter `{key}`"
        );
    }
    let scheme = parsed.scheme();
    let is_loopback = parsed
        .host_str()
        .map(|host| host == "127.0.0.1" || host == "localhost" || host == "::1")
        .unwrap_or(false);
    anyhow::ensure!(
        scheme == "https" || (scheme == "http" && allow_http_for_loopback && is_loopback),
        "MCP OAuth requires https resource URLs except explicit loopback test URLs"
    );
    if parsed.path() == "/" {
        parsed.set_path("");
    }
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

async fn discover_protected_resource_metadata(
    client: &Client,
    resource: &str,
    allow_http_for_loopback: bool,
) -> Result<ProtectedResourceMetadata> {
    let mut candidates = Vec::new();
    if let Ok(response) = client.get(resource).send().await {
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            for value in response
                .headers()
                .get_all(reqwest::header::WWW_AUTHENTICATE)
            {
                if let Ok(header) = value.to_str() {
                    if let Some(url) = parse_www_authenticate_param(header, "resource_metadata") {
                        candidates.push(url);
                    }
                }
            }
        }
    }
    candidates.extend(protected_resource_metadata_candidates(resource)?);
    for candidate in candidates {
        let Ok(candidate) = secure_oauth_url(
            &candidate,
            "resource_metadata",
            allow_http_for_loopback,
            false,
        ) else {
            continue;
        };
        let response = client.get(&candidate).send().await;
        let Ok(response) = response else {
            continue;
        };
        if response.status().is_success() {
            return response
                .json::<ProtectedResourceMetadata>()
                .await
                .with_context(|| {
                    format!("failed to decode MCP protected resource metadata {candidate}")
                });
        }
    }
    bail!("failed to discover MCP protected resource metadata for {resource}");
}

fn protected_resource_metadata_candidates(resource: &str) -> Result<Vec<String>> {
    let parsed = Url::parse(resource)?;
    let origin = parsed
        .origin()
        .ascii_serialization()
        .trim_end_matches('/')
        .to_string();
    let path = parsed.path().trim_start_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource/{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    Ok(candidates)
}

async fn discover_authorization_server_metadata(
    client: &Client,
    issuer: &str,
    allow_http_for_loopback: bool,
) -> Result<AuthorizationServerMetadata> {
    for candidate in authorization_server_metadata_candidates(issuer, allow_http_for_loopback)? {
        let response = client.get(&candidate).send().await;
        let Ok(response) = response else {
            continue;
        };
        if response.status().is_success() {
            let metadata = response
                .json::<AuthorizationServerMetadata>()
                .await
                .with_context(|| {
                    format!("failed to decode OAuth authorization server metadata {candidate}")
                })?;
            anyhow::ensure!(
                metadata.issuer.trim_end_matches('/') == issuer.trim_end_matches('/'),
                "authorization server metadata issuer does not match discovery issuer"
            );
            return Ok(metadata);
        }
    }
    bail!("failed to discover OAuth authorization server metadata for {issuer}");
}

fn authorization_server_metadata_candidates(
    issuer: &str,
    allow_http_for_loopback: bool,
) -> Result<Vec<String>> {
    let issuer = secure_oauth_url(issuer, "issuer", allow_http_for_loopback, true)?;
    let parsed = Url::parse(&issuer).context("invalid OAuth issuer URL")?;
    let origin = parsed
        .origin()
        .ascii_serialization()
        .trim_end_matches('/')
        .to_string();
    let path = parsed.path().trim_matches('/');
    let mut candidates = Vec::new();
    if path.is_empty() {
        candidates.push(format!("{origin}/.well-known/oauth-authorization-server"));
        candidates.push(format!("{origin}/.well-known/openid-configuration"));
    } else {
        candidates.push(format!(
            "{origin}/.well-known/oauth-authorization-server/{path}"
        ));
        candidates.push(format!("{origin}/.well-known/openid-configuration/{path}"));
        candidates.push(format!("{origin}/{path}/.well-known/openid-configuration"));
    }
    Ok(candidates)
}

fn secure_oauth_url(
    value: &str,
    field: &str,
    allow_http_for_loopback: bool,
    trim_trailing_slash: bool,
) -> Result<String> {
    let parsed = Url::parse(value).with_context(|| format!("invalid OAuth {field} URL"))?;
    let host = parsed.host_str().unwrap_or_default();
    let is_loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    anyhow::ensure!(
        parsed.scheme() == "https"
            || (parsed.scheme() == "http" && allow_http_for_loopback && is_loopback),
        "OAuth {field} URL must use https except explicit loopback test URLs"
    );
    anyhow::ensure!(
        parsed.fragment().is_none(),
        "OAuth {field} URL must not contain a fragment"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "OAuth {field} URL must not contain userinfo"
    );
    for (key, _) in parsed.query_pairs() {
        anyhow::ensure!(
            !oauth_url_query_key_is_sensitive(&key),
            "OAuth {field} URL must not contain sensitive query parameter `{key}`"
        );
    }
    if trim_trailing_slash {
        Ok(parsed.to_string().trim_end_matches('/').to_string())
    } else {
        Ok(parsed.to_string())
    }
}

pub(crate) fn oauth_url_query_key_is_sensitive(key: &str) -> bool {
    let normalized = key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let compact = normalized.replace('_', "");
    matches!(
        normalized.as_str(),
        "access_token"
            | "api_key"
            | "authorization"
            | "bearer"
            | "client_secret"
            | "code"
            | "credential"
            | "key"
            | "passphrase"
            | "password"
            | "private_key"
            | "refresh_token"
            | "secret"
            | "signature"
            | "token"
            | "x_api_key"
    ) || matches!(
        compact.as_str(),
        "accesstoken"
            | "apikey"
            | "authorization"
            | "bearer"
            | "clientsecret"
            | "code"
            | "credential"
            | "key"
            | "passphrase"
            | "password"
            | "privatekey"
            | "refreshtoken"
            | "secret"
            | "signature"
            | "token"
            | "xapikey"
    )
}

fn ensure_no_scope_escalation(approved: &[String], returned: &[String]) -> Result<()> {
    for scope in returned {
        anyhow::ensure!(
            approved.iter().any(|approved| approved == scope),
            "MCP OAuth token response attempted to add unapproved scope `{scope}`"
        );
    }
    Ok(())
}

pub fn pkce_s256_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

pub fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

fn parse_www_authenticate_param(header: &str, name: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        let key = key.split_whitespace().last().unwrap_or(key).trim();
        if key != name {
            return None;
        }
        Some(value.trim().trim_matches('"').to_string())
    })
}

fn normalize_scopes(scopes: Vec<String>) -> Vec<String> {
    let mut normalized = scopes
        .into_iter()
        .flat_map(|scope| normalize_scope_string(&scope))
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn normalize_scope_string(scope: &str) -> Vec<String> {
    scope
        .split_whitespace()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{canonical_resource_url, pkce_s256_challenge, secure_oauth_url};

    #[test]
    fn pkce_s256_matches_rfc_example() {
        assert_eq!(
            pkce_s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn canonical_resource_requires_https_except_loopback() {
        assert_eq!(
            canonical_resource_url("https://example.com/mcp/", false).expect("https"),
            "https://example.com/mcp"
        );
        assert!(canonical_resource_url("http://example.com/mcp", false).is_err());
        assert!(
            canonical_resource_url("http://127.0.0.1:8080/mcp", true)
                .expect("loopback")
                .starts_with("http://127.0.0.1:8080/mcp")
        );
    }

    #[test]
    fn canonical_resource_rejects_secret_bearing_url_parts() {
        assert!(canonical_resource_url("https://user:pass@example.com/mcp", false).is_err());
        let error = canonical_resource_url("https://example.com/mcp?access_token=secret", false)
            .expect_err("sensitive query keys should be rejected");
        assert!(
            error
                .to_string()
                .contains("sensitive query parameter `access_token`")
        );
        let error = canonical_resource_url("https://example.com/mcp?accessToken=secret", false)
            .expect_err("camelCase sensitive query keys should be rejected");
        assert!(
            error
                .to_string()
                .contains("sensitive query parameter `accessToken`")
        );
    }

    #[test]
    fn secure_oauth_url_rejects_secret_bearing_url_parts_before_fetch() {
        assert!(
            secure_oauth_url(
                "https://user:pass@example.com/.well-known/oauth-protected-resource",
                "resource_metadata",
                false,
                false
            )
            .is_err()
        );
        let error = secure_oauth_url(
            "https://issuer.example.com/register?client_secret=secret",
            "registration_endpoint",
            false,
            false,
        )
        .expect_err("sensitive query keys should be rejected");
        assert!(
            error
                .to_string()
                .contains("sensitive query parameter `client_secret`")
        );
        let error = secure_oauth_url(
            "https://issuer.example.com/register?clientSecret=secret",
            "registration_endpoint",
            false,
            false,
        )
        .expect_err("camelCase sensitive query keys should be rejected");
        assert!(
            error
                .to_string()
                .contains("sensitive query parameter `clientSecret`")
        );
    }
}
