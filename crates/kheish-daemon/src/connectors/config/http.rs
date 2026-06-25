use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use kheish_types::ReplyHandle;

use super::{ConnectorSessionPolicy, resolve_secret};

const DEFAULT_HTTP_INGRESS_EVENTS_PER_SECOND: u32 = 60;
const DEFAULT_HTTP_SIGNATURE_MAX_AGE_SECS: u64 = 5 * 60;

fn is_false(value: &bool) -> bool {
    !*value
}

/// A generic HTTP webhook ingress connector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpInputConnectorConfig {
    /// Stable connector identifier used in routes.
    pub name: String,
    /// Optional fixed session identifier. When omitted, the payload must provide `session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_session_id: Option<String>,
    /// Default actor identifier when the payload does not provide one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional bearer token required on inbound requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Environment variable containing the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env: Option<String>,
    /// Secret-store slot containing the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_secret_ref: Option<String>,
    /// Optional HMAC-SHA256 signing secret required when `require_hmac_signature=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret: Option<String>,
    /// Environment variable containing the HMAC signing secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret_env: Option<String>,
    /// Secret-store slot containing the HMAC signing secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_secret_secret_ref: Option<String>,
    /// Allows inbound webhook requests without bearer authentication.
    #[serde(default)]
    pub allow_unauthenticated_ingress: bool,
    /// Requires signed inbound webhook requests using X-Kheish-Timestamp and X-Kheish-Signature.
    #[serde(default)]
    pub require_hmac_signature: bool,
    /// Maximum accepted age/skew for signed inbound webhook requests.
    #[serde(default = "default_http_signature_max_age_secs")]
    pub signature_max_age_secs: u64,
    /// Requires each accepted webhook payload to include an idempotency key.
    #[serde(default = "default_true")]
    pub require_idempotency_key: bool,
    /// Maximum accepted ingress requests per second for this connector.
    #[serde(default = "default_http_ingress_events_per_second")]
    pub ingress_events_per_second: u32,
    /// Allows inbound payloads to override reply routing. Disabled by default.
    #[serde(default)]
    pub allow_payload_reply_targets: bool,
    /// Default output targets used when the payload does not provide reply routing.
    #[serde(default)]
    pub default_reply_targets: Vec<ReplyHandle>,
    /// Default opaque binding keys associated with inbound HTTP requests.
    #[serde(default)]
    pub default_binding_keys: Vec<String>,
    /// Controls how this connector materializes daemon sessions for inbound traffic.
    #[serde(default, skip_serializing_if = "ConnectorSessionPolicy::is_empty")]
    pub session_policy: ConnectorSessionPolicy,
}

/// One resolved HTTP webhook connector with secrets loaded.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedHttpInputConnector {
    pub name: String,
    pub fixed_session_id: Option<String>,
    pub actor_id: Option<String>,
    pub bearer_token: Option<String>,
    pub hmac_secret: Option<String>,
    pub allow_unauthenticated_ingress: bool,
    pub require_hmac_signature: bool,
    pub signature_max_age_secs: u64,
    pub require_idempotency_key: bool,
    pub ingress_events_per_second: u32,
    pub allow_payload_reply_targets: bool,
    pub default_reply_targets: Vec<ReplyHandle>,
    pub default_binding_keys: Vec<String>,
    pub session_policy: ConnectorSessionPolicy,
}

impl fmt::Debug for ResolvedHttpInputConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedHttpInputConnector")
            .field("name", &self.name)
            .field("fixed_session_id", &self.fixed_session_id)
            .field("actor_id", &self.actor_id)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "hmac_secret",
                &self.hmac_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "allow_unauthenticated_ingress",
                &self.allow_unauthenticated_ingress,
            )
            .field("require_hmac_signature", &self.require_hmac_signature)
            .field("signature_max_age_secs", &self.signature_max_age_secs)
            .field("require_idempotency_key", &self.require_idempotency_key)
            .field("ingress_events_per_second", &self.ingress_events_per_second)
            .field(
                "allow_payload_reply_targets",
                &self.allow_payload_reply_targets,
            )
            .field("default_reply_targets", &self.default_reply_targets)
            .field("default_binding_keys", &self.default_binding_keys)
            .field("session_policy", &self.session_policy)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpReplyRoute {
    pub url: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_private_network: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

pub(super) fn resolve_connector(
    auth_manager: &kheish_auth::AuthManager,
    config: HttpInputConnectorConfig,
) -> Result<(String, ResolvedHttpInputConnector)> {
    let name = config.name.clone();
    let bearer_token = resolve_secret(
        auth_manager,
        config.bearer_token,
        config.bearer_token_env,
        config.bearer_token_secret_ref,
        &format!("http connector {name} bearer_token"),
    )?;
    let hmac_secret = resolve_secret(
        auth_manager,
        config.hmac_secret,
        config.hmac_secret_env,
        config.hmac_secret_secret_ref,
        &format!("http connector {name} hmac_secret"),
    )?;
    if bearer_token
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        anyhow::bail!("http connector {name} bearer_token cannot be empty");
    }
    if hmac_secret
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        anyhow::bail!("http connector {name} hmac_secret cannot be empty");
    }
    if config.require_hmac_signature && hmac_secret.is_none() {
        anyhow::bail!(
            "http connector {name} requires hmac_secret, hmac_secret_env, or hmac_secret_secret_ref when require_hmac_signature=true"
        );
    }
    let has_hmac_auth = config.require_hmac_signature && hmac_secret.is_some();
    if bearer_token.is_none() && !has_hmac_auth && !config.allow_unauthenticated_ingress {
        anyhow::bail!(
            "http connector {name} requires bearer_token, bearer_token_env, bearer_token_secret_ref, require_hmac_signature=true with hmac_secret, or allow_unauthenticated_ingress=true"
        );
    }
    anyhow::ensure!(
        config.signature_max_age_secs > 0,
        "http connector {name} signature_max_age_secs must be greater than zero"
    );
    anyhow::ensure!(
        config.signature_max_age_secs <= 3600,
        "http connector {name} signature_max_age_secs must be at most 3600"
    );
    anyhow::ensure!(
        !config.require_hmac_signature || config.require_idempotency_key,
        "http connector {name} requires require_idempotency_key=true when require_hmac_signature=true"
    );
    anyhow::ensure!(
        config.ingress_events_per_second > 0,
        "http connector {name} ingress_events_per_second must be greater than zero"
    );
    let resolved = ResolvedHttpInputConnector {
        name: config.name.clone(),
        fixed_session_id: config.fixed_session_id,
        actor_id: config.actor_id,
        bearer_token,
        hmac_secret,
        allow_unauthenticated_ingress: config.allow_unauthenticated_ingress,
        require_hmac_signature: config.require_hmac_signature,
        signature_max_age_secs: config.signature_max_age_secs,
        require_idempotency_key: config.require_idempotency_key,
        ingress_events_per_second: config.ingress_events_per_second,
        allow_payload_reply_targets: config.allow_payload_reply_targets,
        default_reply_targets: config.default_reply_targets,
        default_binding_keys: config.default_binding_keys,
        session_policy: config.session_policy.normalized(),
    };
    Ok((name, resolved))
}

pub(super) fn default_true() -> bool {
    true
}

pub fn default_http_ingress_events_per_second() -> u32 {
    DEFAULT_HTTP_INGRESS_EVENTS_PER_SECOND
}

pub fn default_http_signature_max_age_secs() -> u64 {
    DEFAULT_HTTP_SIGNATURE_MAX_AGE_SECS
}

impl ResolvedHttpInputConnector {
    /// Returns opaque binding keys for one inbound HTTP request.
    pub fn binding_keys(&self, payload_keys: Vec<String>) -> Vec<String> {
        let mut keys = if payload_keys.is_empty() {
            self.default_binding_keys.clone()
        } else {
            payload_keys
        };
        keys.sort();
        keys.dedup();
        keys
    }

    /// Returns a stable fallback session identifier derived from one binding key.
    pub fn fallback_session_id(&self, binding_keys: &[String]) -> Option<String> {
        let key = binding_keys.first()?;
        let digest = Sha256::digest(key.as_bytes());
        Some(format!("http:{}:{}", self.name, hex::encode(&digest[..8])))
    }
}

/// Decodes one HTTP reply route from an opaque reply address or raw URL.
pub fn decode_http_reply_route(address: &str) -> Result<HttpReplyRoute> {
    let route = if address.trim_start().starts_with('{') {
        serde_json::from_str(address).context("invalid http reply route")?
    } else {
        HttpReplyRoute {
            url: address.to_string(),
            allow_private_network: false,
            headers: BTreeMap::new(),
        }
    };
    validate_http_reply_route(&route)?;
    Ok(route)
}

fn validate_http_reply_route(route: &HttpReplyRoute) -> Result<()> {
    let url = reqwest::Url::parse(&route.url).context("invalid http reply target URL")?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("http reply targets must use http:// or https:// addresses");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("http reply targets must not include userinfo");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("http reply target must include a host"))?;
    let lower_host = host.trim_end_matches('.').to_ascii_lowercase();
    if !route.allow_private_network
        && (lower_host == "localhost" || lower_host.ends_with(".localhost"))
    {
        bail!("http reply target host is not allowed");
    }
    if !route.allow_private_network
        && let Some(ip) = parse_http_reply_host_ip(host)
    {
        ensure_public_http_reply_ip(ip)?;
    }
    for header in route.headers.keys() {
        reqwest::header::HeaderName::from_bytes(header.as_bytes())
            .with_context(|| format!("invalid http reply target header name `{header}`"))?;
        let name = header.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "authorization"
                | "connection"
                | "content-length"
                | "content-type"
                | "cookie"
                | "forwarded"
                | "host"
                | "idempotency-key"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "x-api-key"
        ) || name.starts_with("x-forwarded-")
        {
            bail!("http reply target header `{header}` is not allowed");
        }
    }
    for (header, value) in &route.headers {
        reqwest::header::HeaderValue::from_str(value)
            .with_context(|| format!("invalid http reply target header value for `{header}`"))?;
    }
    Ok(())
}

pub(crate) fn ensure_public_http_reply_ip(ip: IpAddr) -> Result<()> {
    match ip {
        IpAddr::V4(value) => {
            ensure_public_ipv4(value)?;
        }
        IpAddr::V6(value) => {
            if let Some(mapped) = ipv4_mapped(value) {
                return ensure_public_ipv4(mapped);
            }
            anyhow::ensure!(
                !(value.is_loopback()
                    || value.is_unspecified()
                    || value.is_multicast()
                    || is_ipv6_unique_local(value)
                    || is_ipv6_unicast_link_local(value)),
                "http reply target IP address is not allowed"
            );
        }
    }
    Ok(())
}

pub(crate) fn parse_http_reply_host_ip(host: &str) -> Option<IpAddr> {
    host_without_ipv6_brackets(host).parse::<IpAddr>().ok()
}

fn ensure_public_ipv4(value: Ipv4Addr) -> Result<()> {
    let octets = value.octets();
    anyhow::ensure!(
        !(value.is_private()
            || value.is_loopback()
            || value.is_link_local()
            || value.is_broadcast()
            || value.is_documentation()
            || octets[0] == 0
            || octets[0] >= 224
            || (octets[0] == 100 && (64..=127).contains(&octets[1]))),
        "http reply target IP address is not allowed"
    );
    Ok(())
}

fn ipv4_mapped(value: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = value.segments();
    if segments[..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff {
        let [high, low] = [segments[6], segments[7]];
        return Some(Ipv4Addr::new(
            (high >> 8) as u8,
            high as u8,
            (low >> 8) as u8,
            low as u8,
        ));
    }
    None
}

fn host_without_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host)
}

fn is_ipv6_unique_local(value: Ipv6Addr) -> bool {
    (value.segments()[0] & 0xfe00) == 0xfc00
}

fn is_ipv6_unicast_link_local(value: Ipv6Addr) -> bool {
    (value.segments()[0] & 0xffc0) == 0xfe80
}

/// Encodes one HTTP reply route into an opaque reply address.
pub fn encode_http_reply_route(route: &HttpReplyRoute) -> String {
    serde_json::to_string(route).expect("http reply route should serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_reply_route_rejects_localhost_aliases_and_mapped_ips() {
        for address in [
            "http://localhost/callback",
            "http://localhost./callback",
            "http://app.localhost./callback",
            "http://127.0.0.1/callback",
            "http://[::1]/callback",
            "http://[::ffff:127.0.0.1]/callback",
        ] {
            assert!(
                decode_http_reply_route(address).is_err(),
                "{address} should be rejected"
            );
        }
    }

    #[test]
    fn http_reply_route_accepts_public_https_targets() {
        let route = decode_http_reply_route("https://example.com/callback").unwrap();
        assert_eq!(route.url, "https://example.com/callback");
    }

    #[test]
    fn http_reply_route_rejects_transport_owned_and_invalid_headers() {
        for header in [
            "Authorization",
            "Forwarded",
            "Idempotency-Key",
            "Content-Length",
            "Transfer-Encoding",
            "X-Forwarded-For",
        ] {
            let encoded = encode_http_reply_route(&HttpReplyRoute {
                url: "https://example.com/callback".to_string(),
                allow_private_network: false,
                headers: BTreeMap::from([(header.to_string(), "value".to_string())]),
            });
            assert!(
                decode_http_reply_route(&encoded).is_err(),
                "{header} should be rejected"
            );
        }

        let encoded = encode_http_reply_route(&HttpReplyRoute {
            url: "https://example.com/callback".to_string(),
            allow_private_network: false,
            headers: BTreeMap::from([("bad header".to_string(), "value".to_string())]),
        });
        assert!(decode_http_reply_route(&encoded).is_err());
    }

    #[test]
    fn http_connector_rejects_empty_secrets() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let auth_manager = kheish_auth::AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let error = resolve_connector(
            auth_manager.as_ref(),
            HttpInputConnectorConfig {
                name: "ingress".to_string(),
                fixed_session_id: None,
                actor_id: None,
                bearer_token: Some(" ".to_string()),
                bearer_token_env: None,
                bearer_token_secret_ref: None,
                hmac_secret: None,
                hmac_secret_env: None,
                hmac_secret_secret_ref: None,
                allow_unauthenticated_ingress: false,
                require_hmac_signature: false,
                signature_max_age_secs: default_http_signature_max_age_secs(),
                require_idempotency_key: true,
                ingress_events_per_second: default_http_ingress_events_per_second(),
                allow_payload_reply_targets: false,
                default_reply_targets: Vec::new(),
                default_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect_err("empty bearer token should fail");
        assert!(
            error.to_string().contains("bearer_token cannot be empty"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn hmac_required_counts_as_auth_and_requires_idempotency() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let auth_manager = kheish_auth::AuthManager::new(temp.path().join("auth-store.json"))
            .expect("auth manager should initialize");
        let (_, resolved) = resolve_connector(
            auth_manager.as_ref(),
            HttpInputConnectorConfig {
                name: "ingress".to_string(),
                fixed_session_id: None,
                actor_id: None,
                bearer_token: None,
                bearer_token_env: None,
                bearer_token_secret_ref: None,
                hmac_secret: Some("hmac-secret".to_string()),
                hmac_secret_env: None,
                hmac_secret_secret_ref: None,
                allow_unauthenticated_ingress: false,
                require_hmac_signature: true,
                signature_max_age_secs: default_http_signature_max_age_secs(),
                require_idempotency_key: true,
                ingress_events_per_second: default_http_ingress_events_per_second(),
                allow_payload_reply_targets: false,
                default_reply_targets: Vec::new(),
                default_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect("HMAC-only connector should be authenticated");
        assert!(resolved.bearer_token.is_none());
        assert!(resolved.require_hmac_signature);

        let error = resolve_connector(
            auth_manager.as_ref(),
            HttpInputConnectorConfig {
                name: "ingress".to_string(),
                fixed_session_id: None,
                actor_id: None,
                bearer_token: None,
                bearer_token_env: None,
                bearer_token_secret_ref: None,
                hmac_secret: Some("hmac-secret".to_string()),
                hmac_secret_env: None,
                hmac_secret_secret_ref: None,
                allow_unauthenticated_ingress: false,
                require_hmac_signature: true,
                signature_max_age_secs: default_http_signature_max_age_secs(),
                require_idempotency_key: false,
                ingress_events_per_second: default_http_ingress_events_per_second(),
                allow_payload_reply_targets: false,
                default_reply_targets: Vec::new(),
                default_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            },
        )
        .expect_err("HMAC without idempotency should fail");
        assert!(
            error
                .to_string()
                .contains("requires require_idempotency_key=true"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn resolved_http_debug_redacts_bearer_token() {
        let resolved = ResolvedHttpInputConnector {
            name: "ingress".to_string(),
            fixed_session_id: None,
            actor_id: None,
            bearer_token: Some("http-secret-token".to_string()),
            hmac_secret: Some("http-hmac-secret".to_string()),
            allow_unauthenticated_ingress: false,
            require_hmac_signature: true,
            signature_max_age_secs: default_http_signature_max_age_secs(),
            require_idempotency_key: true,
            ingress_events_per_second: default_http_ingress_events_per_second(),
            allow_payload_reply_targets: false,
            default_reply_targets: Vec::new(),
            default_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
        };
        let debug = format!("{resolved:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("http-secret-token"));
        assert!(!debug.contains("http-hmac-secret"));
    }
}
