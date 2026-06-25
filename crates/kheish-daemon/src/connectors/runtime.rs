use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::lookup_host;
use tokio::sync::Mutex;

use super::config::{
    ExternalConnectorMode, ResolvedExternalConnector, ensure_public_http_reply_ip,
    parse_http_reply_host_ip,
};
use super::{
    ExternalConnectorDeliveryStatus, ExternalConnectorHealth, ExternalConnectorManifest,
    is_supported_external_connector_protocol_version,
};

const EXTERNAL_MANIFEST_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct ExternalRuntimeMetrics {
    ingress_accepted_total: u64,
    ingress_duplicate_total: u64,
    ingress_rejected_total: u64,
    ingress_rate_limited_total: u64,
    delivery_committed_total: u64,
    delivery_retryable_total: u64,
    delivery_terminal_total: u64,
    child_restarts_total: u64,
    manifest_fetch_failures_total: u64,
}

struct ExternalRateLimiter {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl ExternalRateLimiter {
    fn new(rate_per_second: u32) -> Self {
        let capacity = f64::from(rate_per_second.max(1));
        Self {
            capacity,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    fn reconfigure(&mut self, rate_per_second: u32) {
        let target_capacity = f64::from(rate_per_second.max(1));
        let now = Instant::now();
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + (elapsed * self.capacity)).min(self.capacity);
        if (self.capacity - target_capacity).abs() > f64::EPSILON {
            self.capacity = target_capacity;
            self.tokens = self.tokens.min(self.capacity);
        }
    }

    fn take(&mut self, rate_per_second: u32) -> Option<u64> {
        self.reconfigure(rate_per_second);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let missing = 1.0 - self.tokens;
            Some(((missing / self.capacity) * 1000.0).ceil().max(1.0) as u64)
        }
    }
}

struct ExternalRuntimeEntry {
    base_url: String,
    manifest: Option<ExternalConnectorManifest>,
    manifest_fetched_at: Option<Instant>,
    health: Option<ExternalConnectorHealth>,
    child_process_credential_token: Option<String>,
    limiter: ExternalRateLimiter,
    metrics: ExternalRuntimeMetrics,
}

impl ExternalRuntimeEntry {
    fn new(connector: &ResolvedExternalConnector) -> Self {
        Self {
            base_url: connector.base_url.clone(),
            manifest: None,
            manifest_fetched_at: None,
            health: None,
            child_process_credential_token: None,
            limiter: ExternalRateLimiter::new(connector.ingress_events_per_second),
            metrics: ExternalRuntimeMetrics::default(),
        }
    }

    fn reset_for(&mut self, connector: &ResolvedExternalConnector) {
        if self.base_url != connector.base_url {
            self.base_url = connector.base_url.clone();
            self.manifest = None;
            self.manifest_fetched_at = None;
            self.health = None;
            self.child_process_credential_token = None;
        }
        self.limiter
            .reconfigure(connector.ingress_events_per_second);
    }

    fn note_manifest(&mut self, manifest: ExternalConnectorManifest) {
        self.manifest = Some(manifest);
        self.manifest_fetched_at = Some(Instant::now());
    }

    fn cached_manifest(&self) -> Option<ExternalConnectorManifest> {
        if self
            .manifest_fetched_at
            .is_some_and(|fetched_at| fetched_at.elapsed() <= EXTERNAL_MANIFEST_CACHE_TTL)
        {
            return self.manifest.clone();
        }
        None
    }
}

pub(crate) struct ExternalConnectorRuntimeService {
    client: reqwest::Client,
    entries: Mutex<BTreeMap<String, ExternalRuntimeEntry>>,
}

impl ExternalConnectorRuntimeService {
    pub(crate) fn new() -> Self {
        let client = external_runtime_http_client_builder()
            .build()
            .expect("external connector runtime client should build");
        Self {
            client,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    async fn with_entry<T>(
        &self,
        connector: &ResolvedExternalConnector,
        update: impl FnOnce(&mut ExternalRuntimeEntry) -> T,
    ) -> T {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .entry(connector.name.clone())
            .or_insert_with(|| ExternalRuntimeEntry::new(connector));
        entry.reset_for(connector);
        update(entry)
    }

    pub(crate) async fn note_manifest(
        &self,
        connector: &ResolvedExternalConnector,
        manifest: ExternalConnectorManifest,
    ) {
        self.with_entry(connector, |entry| {
            entry.note_manifest(manifest);
        })
        .await;
    }

    pub(crate) async fn note_health(
        &self,
        connector: &ResolvedExternalConnector,
        health: ExternalConnectorHealth,
    ) {
        self.with_entry(connector, |entry| {
            entry.health = Some(health);
        })
        .await;
    }

    pub(crate) async fn set_child_process_credential_token(
        &self,
        connector: &ResolvedExternalConnector,
        token: Option<String>,
    ) {
        self.with_entry(connector, |entry| {
            entry.child_process_credential_token = token;
        })
        .await;
    }

    pub(crate) async fn child_process_credential_token(
        &self,
        connector: &ResolvedExternalConnector,
    ) -> Option<String> {
        self.with_entry(connector, |entry| {
            entry.child_process_credential_token.clone()
        })
        .await
    }

    pub(crate) async fn allow_ingress(
        &self,
        connector: &ResolvedExternalConnector,
    ) -> std::result::Result<(), u64> {
        self.with_entry(connector, |entry| {
            entry.limiter.take(connector.ingress_events_per_second)
        })
        .await
        .map_or(Ok(()), Err)
    }

    pub(crate) async fn note_ingress_accepted(&self, connector: &ResolvedExternalConnector) {
        self.with_entry(connector, |entry| entry.metrics.ingress_accepted_total += 1)
            .await;
    }

    pub(crate) async fn note_ingress_duplicate(&self, connector: &ResolvedExternalConnector) {
        self.with_entry(connector, |entry| {
            entry.metrics.ingress_duplicate_total += 1
        })
        .await;
    }

    pub(crate) async fn note_ingress_rejected(&self, connector: &ResolvedExternalConnector) {
        self.with_entry(connector, |entry| entry.metrics.ingress_rejected_total += 1)
            .await;
    }

    pub(crate) async fn note_ingress_rate_limited(&self, connector: &ResolvedExternalConnector) {
        self.with_entry(connector, |entry| {
            entry.metrics.ingress_rate_limited_total += 1
        })
        .await;
    }

    pub(crate) async fn note_delivery_status(
        &self,
        connector: &ResolvedExternalConnector,
        status: ExternalConnectorDeliveryStatus,
    ) {
        self.with_entry(connector, |entry| match status {
            ExternalConnectorDeliveryStatus::Committed => {
                entry.metrics.delivery_committed_total += 1;
            }
            ExternalConnectorDeliveryStatus::RetryableError => {
                entry.metrics.delivery_retryable_total += 1;
            }
            ExternalConnectorDeliveryStatus::TerminalError => {
                entry.metrics.delivery_terminal_total += 1;
            }
        })
        .await;
    }

    pub(crate) async fn note_child_restart(&self, connector: &ResolvedExternalConnector) {
        self.with_entry(connector, |entry| entry.metrics.child_restarts_total += 1)
            .await;
    }

    pub(crate) async fn note_manifest_fetch_failure(&self, connector: &ResolvedExternalConnector) {
        self.with_entry(connector, |entry| {
            entry.metrics.manifest_fetch_failures_total += 1
        })
        .await;
    }

    async fn fetch_manifest(
        &self,
        connector: &ResolvedExternalConnector,
    ) -> Result<ExternalConnectorManifest> {
        let client = self.client_for_connector(connector).await?;
        let mut request = client.get(format!("{}/manifest", connector.base_url));
        if let Some(shared_token) = connector.shared_token.as_deref() {
            request = request.bearer_auth(shared_token);
        }
        let manifest = request
            .send()
            .await?
            .error_for_status()?
            .json::<ExternalConnectorManifest>()
            .await?;
        if !is_supported_external_connector_protocol_version(manifest.protocol_version) {
            self.note_manifest_fetch_failure(connector).await;
            return Err(anyhow!(
                "external connector {} reported unsupported protocol_version {}",
                connector.name,
                manifest.protocol_version
            ));
        }
        self.note_manifest(connector, manifest.clone()).await;
        Ok(manifest)
    }

    async fn client_for_connector(
        &self,
        connector: &ResolvedExternalConnector,
    ) -> Result<reqwest::Client> {
        if connector.allow_private_network {
            return Ok(self.client.clone());
        }
        let parsed = reqwest::Url::parse(&connector.base_url)
            .context("invalid external connector base_url")?;
        let host = parsed.host_str().ok_or_else(|| {
            anyhow!(
                "external connector {} base_url is missing a host",
                connector.name
            )
        })?;
        if let Some(ip) = parse_http_reply_host_ip(host) {
            ensure_public_http_reply_ip(ip).with_context(|| {
                format!(
                    "external connector {} base_url targets a private network address",
                    connector.name
                )
            })?;
            return Ok(self.client.clone());
        }
        let port = parsed.port_or_known_default().ok_or_else(|| {
            anyhow!(
                "external connector {} base_url must include a resolvable port",
                connector.name
            )
        })?;
        let addrs = lookup_host((host, port))
            .await
            .with_context(|| {
                format!(
                    "failed to resolve external connector {} base_url host {host}",
                    connector.name
                )
            })?
            .collect::<Vec<_>>();
        if addrs.is_empty() {
            bail!(
                "external connector {} base_url host {host} did not resolve to any address",
                connector.name
            );
        }
        for addr in &addrs {
            ensure_public_http_reply_ip(addr.ip()).with_context(|| {
                format!(
                    "external connector {} base_url host {host} resolved to a private network address",
                    connector.name
                )
            })?;
        }
        Ok(external_runtime_http_client_with_resolved_host(
            host, &addrs,
        ))
    }

    pub(crate) async fn ensure_manifest(
        &self,
        connector: &ResolvedExternalConnector,
    ) -> Result<ExternalConnectorManifest> {
        if let Some(cached) = self
            .with_entry(connector, |entry| entry.cached_manifest())
            .await
        {
            return Ok(cached);
        }
        self.fetch_manifest(connector).await
    }

    pub(crate) async fn validate_child_process_instance_id(
        &self,
        connector: &ResolvedExternalConnector,
        observed_instance_id: &str,
    ) -> Result<()> {
        if connector.mode != ExternalConnectorMode::ChildProcess {
            return Ok(());
        }
        let cached = self
            .with_entry(connector, |entry| entry.cached_manifest())
            .await;
        if cached
            .as_ref()
            .is_some_and(|manifest| manifest.instance_id == observed_instance_id)
        {
            return Ok(());
        }
        let manifest = self.fetch_manifest(connector).await?;
        anyhow::ensure!(
            manifest.instance_id == observed_instance_id,
            "external connector {} instance_id mismatch: expected {}, got {}",
            connector.name,
            manifest.instance_id,
            observed_instance_id
        );
        Ok(())
    }

    pub(crate) async fn render_prometheus_metrics(&self) -> String {
        let entries = self.entries.lock().await;
        let mut output = String::new();
        output.push_str("# TYPE kheish_external_connector_ingress_total counter\n");
        output.push_str("# TYPE kheish_external_connector_ingress_rate_limited_total counter\n");
        output.push_str("# TYPE kheish_external_connector_delivery_total counter\n");
        output.push_str("# TYPE kheish_external_connector_child_restarts_total counter\n");
        output.push_str("# TYPE kheish_external_connector_manifest_fetch_failures_total counter\n");
        for (name, entry) in entries.iter() {
            let labels = format!("connector=\"{}\"", prometheus_label_value(name));
            let _ = writeln!(
                output,
                "kheish_external_connector_ingress_total{{{labels},status=\"accepted\"}} {}",
                entry.metrics.ingress_accepted_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_ingress_total{{{labels},status=\"duplicate\"}} {}",
                entry.metrics.ingress_duplicate_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_ingress_total{{{labels},status=\"rejected\"}} {}",
                entry.metrics.ingress_rejected_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_ingress_rate_limited_total{{{labels}}} {}",
                entry.metrics.ingress_rate_limited_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_delivery_total{{{labels},status=\"committed\"}} {}",
                entry.metrics.delivery_committed_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_delivery_total{{{labels},status=\"retryable\"}} {}",
                entry.metrics.delivery_retryable_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_delivery_total{{{labels},status=\"terminal\"}} {}",
                entry.metrics.delivery_terminal_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_child_restarts_total{{{labels}}} {}",
                entry.metrics.child_restarts_total
            );
            let _ = writeln!(
                output,
                "kheish_external_connector_manifest_fetch_failures_total{{{labels}}} {}",
                entry.metrics.manifest_fetch_failures_total
            );
        }
        output
    }
}

fn external_runtime_http_client_with_resolved_host(
    host: &str,
    addrs: &[SocketAddr],
) -> reqwest::Client {
    external_runtime_http_client_builder()
        .resolve_to_addrs(host, addrs)
        .build()
        .expect("external runtime HTTP client with resolved host should build")
}

fn external_runtime_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
}

fn prometheus_label_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use crate::connectors::ExternalConnectorMode;
    use crate::connectors::config::ConnectorSessionPolicy;

    use super::{
        EXTERNAL_MANIFEST_CACHE_TTL, ExternalConnectorRuntimeService, ExternalRuntimeEntry,
        ResolvedExternalConnector,
    };

    fn test_connector(base_url: &str, allow_private_network: bool) -> ResolvedExternalConnector {
        ResolvedExternalConnector {
            name: "discord".to_string(),
            platform: "discord".to_string(),
            mode: ExternalConnectorMode::RemoteHttp,
            base_url: base_url.to_string(),
            allow_private_network,
            shared_token: Some("secret".to_string()),
            allow_unauthenticated_ingress: false,
            fixed_session_id: None,
            include_self_output: true,
            additional_reply_targets: Vec::new(),
            additional_binding_keys: Vec::new(),
            session_policy: ConnectorSessionPolicy::default(),
            ingress_events_per_second: 100,
            child_process: None,
        }
    }

    #[tokio::test]
    async fn external_runtime_rejects_private_base_url_even_if_persisted() {
        let service = ExternalConnectorRuntimeService::new();
        let error = service
            .ensure_manifest(&test_connector("http://127.0.0.1:9", false))
            .await
            .expect_err("private remote base_url should fail before manifest fetch");
        assert!(
            error.to_string().contains("private network address"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn external_manifest_cache_expires_after_ttl() {
        let connector = test_connector("https://example.com", false);
        let mut entry = ExternalRuntimeEntry::new(&connector);
        entry.note_manifest(crate::connectors::ExternalConnectorManifest {
            protocol_version: 1,
            instance_id: "instance-1".to_string(),
            capabilities: crate::connectors::ExternalConnectorCapabilities::default(),
            experimental: false,
        });
        assert!(entry.cached_manifest().is_some());
        entry.manifest_fetched_at = Some(
            std::time::Instant::now()
                .checked_sub(EXTERNAL_MANIFEST_CACHE_TTL + std::time::Duration::from_secs(1))
                .expect("test instant should support subtraction"),
        );
        assert!(entry.cached_manifest().is_none());
    }
}
