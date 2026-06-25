//! Output plugin implementations for daemon-managed connectors.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use kheish_output::OutputHost;
use kheish_runtime::external_action_trace;
use kheish_runtime::{RuntimeObserver, failed_reqwest_external_action_outcome};
use serde::Serialize;

use super::config::ConnectorRegistry;
use super::runtime::ExternalConnectorRuntimeService;
use crate::assets::FileAssetStore;
use crate::delivery::{
    DeliveryDispatcher, DeliveryQueue, QueuedOutputPlugin, is_terminal_delivery_error,
    retry_after_delivery_error_ms,
};

mod external;
mod http;
mod slack;
mod telegram;

pub fn build_delivery_dispatcher(
    connectors: Arc<ConnectorRegistry>,
    runtime: Arc<ExternalConnectorRuntimeService>,
    assets: Arc<FileAssetStore>,
    observer: Arc<dyn RuntimeObserver>,
    state_root: PathBuf,
) -> Arc<DeliveryDispatcher> {
    let mut dispatcher = DeliveryDispatcher::new();
    dispatcher.register("http", http::HttpOutputPlugin::new(observer.clone()));
    dispatcher.register(
        "external",
        external::ExternalOutputPlugin::new(connectors.clone(), runtime, observer.clone()),
    );
    dispatcher.register(
        "slack",
        slack::SlackOutputPlugin::new(
            connectors.clone(),
            assets.clone(),
            observer.clone(),
            state_root.join("slack-delivery-progress.d"),
        ),
    );
    dispatcher.register(
        "telegram",
        telegram::TelegramOutputPlugin::new(
            connectors,
            assets,
            observer,
            state_root.join("telegram-delivery-progress.d"),
        ),
    );
    Arc::new(dispatcher)
}

pub fn register_output_plugins(host: &mut OutputHost, queue: Arc<DeliveryQueue>) {
    host.register(QueuedOutputPlugin::new("http", queue.clone()));
    host.register(QueuedOutputPlugin::new("external", queue.clone()));
    host.register(QueuedOutputPlugin::new("slack", queue.clone()));
    host.register(QueuedOutputPlugin::new("telegram", queue));
}

pub(super) fn output_http_client() -> reqwest::Client {
    output_http_client_builder()
        .build()
        .expect("output HTTP client should build")
}

pub(super) fn output_http_client_with_resolved_host(
    host: &str,
    addrs: &[SocketAddr],
) -> reqwest::Client {
    output_http_client_builder()
        .resolve_to_addrs(host, addrs)
        .build()
        .expect("output HTTP client with resolved host should build")
}

fn output_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
}

pub(super) struct OutputAuditSpan {
    observer: Arc<dyn RuntimeObserver>,
    target: String,
    request_digest: String,
}

impl OutputAuditSpan {
    pub(super) fn start<T>(
        observer: Arc<dyn RuntimeObserver>,
        target: impl Into<String>,
        request: &T,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let target = target.into();
        let request_digest = kheish_codec::digest_serialize(request)?;
        observer.record_external_action(external_action_trace(
            "request",
            "connector_delivery",
            target.clone(),
            Some(request_digest.clone()),
            None,
            None,
        ))?;
        Ok(Self {
            observer,
            target,
            request_digest,
        })
    }

    pub(super) fn record_success<T>(&self, response: &T, outcome: impl Into<String>) -> Result<()>
    where
        T: Serialize,
    {
        kheish_codec::digest_serialize(response).and_then(|response_digest| {
            self.observer.record_external_action(external_action_trace(
                "response",
                "connector_delivery",
                self.target.clone(),
                Some(self.request_digest.clone()),
                Some(response_digest),
                Some(outcome.into()),
            ))
        })
    }

    pub(super) fn record_failure(&self, error: &anyhow::Error) -> Result<()> {
        self.observer.record_external_action(external_action_trace(
            "response",
            "connector_delivery",
            self.target.clone(),
            Some(self.request_digest.clone()),
            None,
            Some(safe_delivery_failure_outcome(error)),
        ))
    }
}

pub(super) fn safe_url_target(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let Some(host) = parsed.host_str() else {
                return "unknown-host".to_string();
            };
            let mut target = format!("{}://{}", parsed.scheme(), host);
            if let Some(port) = parsed.port() {
                target.push(':');
                target.push_str(&port.to_string());
            }
            target
        }
        Err(_) => {
            let digest = kheish_codec::digest_text(url);
            format!(
                "invalid_url_sha256:{}",
                digest.get(..16).unwrap_or(digest.as_str())
            )
        }
    }
}

fn safe_delivery_failure_outcome(error: &anyhow::Error) -> String {
    if is_terminal_delivery_error(error) {
        return "failed:terminal".to_string();
    }
    if retry_after_delivery_error_ms(error).is_some() {
        return "failed:rate_limited".to_string();
    }
    if let Some(reqwest_error) = error.downcast_ref::<reqwest::Error>() {
        return failed_reqwest_external_action_outcome(reqwest_error);
    }
    let message = error.to_string();
    if message.contains("timed out") {
        return "failed:timeout".to_string();
    }
    if message.contains("status") || message.contains("HTTP ") {
        return "failed:status".to_string();
    }
    if message.contains("decode") {
        return "failed:decode".to_string();
    }
    "failed:internal".to_string()
}

pub(super) fn summarize_delivery_target(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= 160 {
        normalized
    } else {
        format!("{}...", normalized.chars().take(160).collect::<String>())
    }
}

pub(super) fn stable_target_digest(value: &str) -> String {
    let digest = kheish_codec::digest_text(value);
    digest.get(..16).unwrap_or(digest.as_str()).to_string()
}

#[cfg(test)]
mod tests {
    use crate::delivery::{retry_after_delivery_error, terminal_delivery_error};

    use super::{safe_delivery_failure_outcome, safe_url_target, stable_target_digest};

    #[test]
    fn safe_url_target_redacts_path_query_fragment_and_userinfo() {
        assert_eq!(
            safe_url_target("https://user:secret@example.com:8443/path/token?api_key=secret#frag"),
            "https://example.com:8443"
        );
        let invalid = safe_url_target("not a url with secret-token");
        assert!(invalid.starts_with("invalid_url_sha256:"));
        assert!(!invalid.contains("secret-token"));
    }

    #[test]
    fn stable_target_digest_is_short_and_stable() {
        let first = stable_target_digest("C_OPS_SECRETISH");
        let second = stable_target_digest("C_OPS_SECRETISH");
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
        assert!(!first.contains("C_OPS_SECRETISH"));
    }

    #[test]
    fn output_audit_failure_outcome_preserves_delivery_error_class() {
        let rate_limited = retry_after_delivery_error(1_000, "downstream asked for retry");
        assert_eq!(
            safe_delivery_failure_outcome(&rate_limited),
            "failed:rate_limited"
        );

        let terminal = terminal_delivery_error("unknown delivery transport old-plugin");
        assert_eq!(safe_delivery_failure_outcome(&terminal), "failed:terminal");
    }
}
