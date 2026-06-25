use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_output::{OutputManifest, OutputPlugin, ResponseEnvelope};
use kheish_runtime::RuntimeObserver;
use kheish_types::{AttachmentRef, ContentPart};
use serde_json::{Value, json};
use tokio::net::lookup_host;

use crate::connectors::config::{
    ConnectorRegistry, ResolvedExternalConnector, decode_external_reply_route,
    ensure_public_http_reply_ip, parse_http_reply_host_ip,
};
use crate::connectors::{
    ExternalConnectorAssetDescriptor, ExternalConnectorDeliveryRequest,
    ExternalConnectorDeliveryResponse, ExternalConnectorDeliveryStatus,
    ExternalConnectorOutputPart, ExternalConnectorRuntimeService,
};
use crate::delivery::{DeliveryTransport, retry_after_delivery_error, terminal_delivery_error};

use super::{
    OutputAuditSpan, output_http_client, output_http_client_with_resolved_host,
    summarize_delivery_target,
};

const MAX_EXTERNAL_DELIVERY_RESPONSE_BYTES: usize = 64 * 1024;

pub struct ExternalOutputPlugin {
    connectors: Arc<ConnectorRegistry>,
    runtime: Arc<ExternalConnectorRuntimeService>,
    client: reqwest::Client,
    observer: Arc<dyn RuntimeObserver>,
}

impl ExternalOutputPlugin {
    pub fn new(
        connectors: Arc<ConnectorRegistry>,
        runtime: Arc<ExternalConnectorRuntimeService>,
        observer: Arc<dyn RuntimeObserver>,
    ) -> Self {
        Self {
            connectors,
            runtime,
            client: output_http_client(),
            observer,
        }
    }
}

#[async_trait]
impl OutputPlugin for ExternalOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        OutputManifest {
            name: "external".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "External sidecar output".to_string(),
        }
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        let reply = response
            .reply
            .as_ref()
            .ok_or_else(|| anyhow!("external output requires one reply target"))?;
        let route = decode_external_reply_route(&reply.address)?;
        let connector = self
            .connectors
            .external(&route.connector)
            .ok_or_else(|| anyhow!("unknown external connector {}", route.connector))?;
        let delivery_id = response
            .metadata
            .get("delivery_id")
            .and_then(Value::as_str)
            .unwrap_or("delivery-unknown")
            .to_string();
        let attempt = response
            .metadata
            .get("delivery_attempt")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        let manifest = self.runtime.ensure_manifest(&connector).await?;
        if !manifest.capabilities.attachments_out
            && (response
                .parts
                .iter()
                .any(content_part_requires_asset_download)
                || !response.artifacts.is_empty())
        {
            self.runtime
                .note_delivery_status(&connector, ExternalConnectorDeliveryStatus::TerminalError)
                .await;
            return Err(terminal_delivery_error(format!(
                "external connector {} does not advertise attachments_out support",
                connector.name
            )));
        }
        if connector.shared_token.is_none()
            && (response
                .parts
                .iter()
                .any(content_part_requires_asset_download)
                || !response.artifacts.is_empty())
        {
            return Err(terminal_delivery_error(format!(
                "external connector {} requires shared_token to deliver attachments",
                connector.name
            )));
        }
        let parts = response
            .parts
            .iter()
            .map(|part| external_output_part(&connector.name, &delivery_id, part))
            .collect();
        let artifacts = response
            .artifacts
            .iter()
            .map(|attachment| external_asset_descriptor(&connector.name, &delivery_id, attachment))
            .collect();
        let body = ExternalConnectorDeliveryRequest {
            protocol_version: manifest.protocol_version,
            delivery_id,
            attempt,
            reply_route: route.route,
            conversation: response.conversation.clone(),
            content: response.content.clone(),
            parts,
            artifacts,
            metadata: response.metadata.clone(),
        };

        let audit = OutputAuditSpan::start(
            self.observer.clone(),
            format!(
                "external:{}:/deliver",
                summarize_delivery_target(&connector.name)
            ),
            &body,
        )?;
        let delivery = async {
            let client = self.client_for_connector(&connector).await?;
            let mut request = client.post(format!("{}/deliver", connector.base_url));
            if let Some(shared_token) = connector.shared_token.as_deref() {
                request = request.bearer_auth(shared_token);
            }
            request = request
                .header("Idempotency-Key", format!("kheish:{}", body.delivery_id))
                .header(
                    "X-Kheish-External-Protocol-Version",
                    body.protocol_version.to_string(),
                );
            let result = request.json(&body).send().await.with_context(|| {
                format!("failed to deliver external output via {}", connector.name)
            })?;
            let status = result.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after_ms = external_retry_after_ms(result.headers()).unwrap_or(1_000);
                return Err(retry_after_delivery_error(
                    retry_after_ms,
                    format!("external connector {} returned 429", connector.name),
                ));
            }
            let response_bytes =
                read_external_delivery_response_body_limited(result, &connector.name)
                    .await
                    .context("failed to read external connector delivery response body")?;
            let response =
                external_delivery_response_from_http(&connector.name, status, &response_bytes)?;
            Ok((status.as_u16(), response))
        }
        .await;
        let (http_status, response) = match delivery {
            Ok(value) => value,
            Err(error) => {
                audit.record_failure(&error)?;
                return Err(error);
            }
        };
        self.runtime
            .note_delivery_status(&connector, response.status)
            .await;
        let response_summary = json!({
            "http_status": http_status,
            "delivery_status": response.status,
            "detail": response.detail,
        });
        match response.status {
            ExternalConnectorDeliveryStatus::Committed => {
                audit.record_success(&response_summary, "committed")?;
                Ok(())
            }
            ExternalConnectorDeliveryStatus::RetryableError => {
                audit.record_success(&response_summary, "retryable_error")?;
                Err(anyhow!(
                    "{}",
                    response
                        .detail
                        .unwrap_or_else(|| "external delivery requested a retry".to_string())
                ))
            }
            ExternalConnectorDeliveryStatus::TerminalError => {
                audit.record_success(&response_summary, "terminal_error")?;
                Err(terminal_delivery_error(response.detail.unwrap_or_else(
                    || "external delivery failed permanently".to_string(),
                )))
            }
        }
    }
}

impl ExternalOutputPlugin {
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
        Ok(output_http_client_with_resolved_host(host, &addrs))
    }
}

fn external_output_part(
    connector_name: &str,
    delivery_id: &str,
    part: &ContentPart,
) -> ExternalConnectorOutputPart {
    match part {
        ContentPart::Text { text } => ExternalConnectorOutputPart::Text { text: text.clone() },
        ContentPart::Attachment { attachment } => ExternalConnectorOutputPart::Attachment {
            attachment: external_asset_descriptor(connector_name, delivery_id, attachment),
        },
    }
}

fn external_asset_descriptor(
    connector_name: &str,
    delivery_id: &str,
    attachment: &AttachmentRef,
) -> ExternalConnectorAssetDescriptor {
    ExternalConnectorAssetDescriptor {
        id: attachment.id.clone(),
        media_type: attachment.media_type.clone(),
        file_name: attachment.file_name.clone(),
        sha256: attachment.sha256.clone(),
        byte_length: attachment.byte_length,
        download_path: format!(
            "/v1/connectors/external/{connector_name}/deliveries/{delivery_id}/assets/{}/raw",
            attachment.id,
        ),
    }
}

fn content_part_requires_asset_download(part: &ContentPart) -> bool {
    matches!(part, ContentPart::Attachment { .. })
}

async fn read_external_delivery_response_body_limited(
    mut response: reqwest::Response,
    connector_name: &str,
) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > MAX_EXTERNAL_DELIVERY_RESPONSE_BYTES as u64
    {
        bail!(
            "external connector {connector_name} delivery response exceeded {MAX_EXTERNAL_DELIVERY_RESPONSE_BYTES} bytes"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_EXTERNAL_DELIVERY_RESPONSE_BYTES {
            bail!(
                "external connector {connector_name} delivery response exceeded {MAX_EXTERNAL_DELIVERY_RESPONSE_BYTES} bytes"
            );
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn external_delivery_response_from_http(
    connector_name: &str,
    status: reqwest::StatusCode,
    response_bytes: &[u8],
) -> Result<ExternalConnectorDeliveryResponse> {
    let decoded = serde_json::from_slice::<ExternalConnectorDeliveryResponse>(response_bytes);
    match (status.is_success(), decoded) {
        (true, Ok(response)) => Ok(response),
        (false, Ok(response)) => {
            if response.status == ExternalConnectorDeliveryStatus::Committed {
                return Ok(external_delivery_error_from_http(
                    connector_name,
                    status,
                    Some("returned committed delivery status on non-success HTTP response"),
                ));
            }
            Ok(response)
        }
        (false, Err(_)) => Ok(external_delivery_error_from_http(
            connector_name,
            status,
            None,
        )),
        (true, Err(error)) => {
            Err(error).context("failed to decode external connector delivery response")
        }
    }
}

fn external_delivery_error_from_http(
    connector_name: &str,
    status: reqwest::StatusCode,
    detail: Option<&str>,
) -> ExternalConnectorDeliveryResponse {
    let delivery_status = if status.is_client_error() {
        ExternalConnectorDeliveryStatus::TerminalError
    } else {
        ExternalConnectorDeliveryStatus::RetryableError
    };
    let suffix = detail
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    ExternalConnectorDeliveryResponse {
        status: delivery_status,
        detail: Some(format!(
            "external connector {connector_name} returned HTTP {status} during delivery{suffix}"
        )),
    }
}

fn external_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after_header_ms)
}

fn parse_retry_after_header_ms(value: &str) -> Option<u64> {
    const MAX_RETRY_AFTER_MS: u64 = 60 * 60 * 1_000;
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1000).clamp(1, MAX_RETRY_AFTER_MS));
    }
    let target = chrono::DateTime::parse_from_rfc2822(trimmed).ok()?;
    let delay_ms = target
        .timestamp_millis()
        .saturating_sub(crate::now_ms() as i64);
    Some((delay_ms.max(1) as u64).min(MAX_RETRY_AFTER_MS))
}

#[async_trait]
impl DeliveryTransport for ExternalOutputPlugin {
    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        <Self as OutputPlugin>::deliver(self, response).await
    }
}

#[cfg(test)]
mod tests {
    use kheish_types::ConversationKey;
    use serde_json::json;

    use crate::connectors::ExternalConnectorDeliveryRequest;

    use super::*;

    #[test]
    fn external_output_retry_after_parses_seconds_header() {
        assert_eq!(parse_retry_after_header_ms("5"), Some(5000));
        assert_eq!(parse_retry_after_header_ms(" 1 "), Some(1000));
        assert_eq!(parse_retry_after_header_ms("999999"), Some(3_600_000));
    }

    #[test]
    fn external_output_429_without_retry_after_is_retryable() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(external_retry_after_ms(&headers).unwrap_or(1_000), 1_000);
    }

    #[test]
    fn external_output_retry_after_parses_http_date_header() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(60);
        let header = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let retry_after =
            parse_retry_after_header_ms(&header).expect("HTTP-date Retry-After should parse");
        assert_eq!(
            (1..=60_000).contains(&retry_after),
            true,
            "unexpected retry_after: {retry_after}"
        );
    }

    #[test]
    fn external_delivery_rejects_committed_status_on_http_error() {
        let response = external_delivery_response_from_http(
            "discord",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"status":"committed"}"#,
        )
        .unwrap();
        assert_eq!(
            response.status,
            ExternalConnectorDeliveryStatus::RetryableError
        );
        assert!(
            response
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("committed delivery status")),
            "unexpected detail: {response:?}"
        );
    }

    #[test]
    fn external_delivery_honors_terminal_status_on_http_error() {
        let response = external_delivery_response_from_http(
            "discord",
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"status":"terminal_error","detail":"bad route"}"#,
        )
        .unwrap();
        assert_eq!(
            response.status,
            ExternalConnectorDeliveryStatus::TerminalError
        );
        assert_eq!(response.detail.as_deref(), Some("bad route"));
    }

    #[test]
    fn external_delivery_request_golden_v1_shape_is_stable() {
        let request = ExternalConnectorDeliveryRequest {
            protocol_version: 1,
            delivery_id: "delivery-1".to_string(),
            attempt: 2,
            reply_route: "thread-1".to_string(),
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-1", "delivery_attempt": 2 }),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            encoded,
            json!({
                "protocol_version": 1,
                "delivery_id": "delivery-1",
                "attempt": 2,
                "reply_route": "thread-1",
                "conversation": {
                    "session_id": "session-1",
                    "thread_id": null
                },
                "content": "hello",
                "metadata": {
                    "delivery_id": "delivery-1",
                    "delivery_attempt": 2
                }
            })
        );
    }
}
