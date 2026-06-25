use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use kheish_output::{OutputManifest, OutputPlugin, ResponseEnvelope};
use kheish_runtime::RuntimeObserver;
use kheish_types::{AttachmentRef, ContentPart};
use serde::Serialize;
use serde_json::json;
use tokio::net::lookup_host;

use crate::connectors::config::{
    decode_http_reply_route, ensure_public_http_reply_ip, parse_http_reply_host_ip,
};
use crate::delivery::{DeliveryTransport, retry_after_delivery_error, terminal_delivery_error};

use super::{
    OutputAuditSpan, output_http_client, output_http_client_with_resolved_host, safe_url_target,
};

pub struct HttpOutputPlugin {
    client: reqwest::Client,
    observer: Arc<dyn RuntimeObserver>,
}

impl HttpOutputPlugin {
    pub fn new(observer: Arc<dyn RuntimeObserver>) -> Self {
        Self {
            client: output_http_client(),
            observer,
        }
    }
}

#[derive(Debug, Serialize)]
struct HttpAttachmentDescriptor<'a> {
    id: &'a str,
    media_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_length: Option<u64>,
    download_path: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HttpOutputPart<'a> {
    Text {
        text: &'a str,
    },
    Attachment {
        attachment: HttpAttachmentDescriptor<'a>,
    },
}

#[async_trait]
impl OutputPlugin for HttpOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        OutputManifest {
            name: "http".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "HTTP webhook output".to_string(),
        }
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        let reply = response
            .reply
            .as_ref()
            .ok_or_else(|| anyhow!("http output requires one reply target"))?;
        let route = decode_http_reply_route(&reply.address)?;
        let client = self.client_for_route(&route).await?;
        let safe_target = safe_url_target(&route.url);
        let mut request = client.post(&route.url);
        for (name, value) in route.headers {
            request = request.header(name, value);
        }
        if let Some(delivery_id) = http_delivery_id(&response) {
            request = request.header("Idempotency-Key", format!("kheish:{delivery_id}"));
        }
        let resolved_parts = response
            .parts
            .iter()
            .map(render_http_part)
            .collect::<Vec<_>>();
        let artifact_attachments = response
            .artifacts
            .iter()
            .map(http_attachment_descriptor)
            .collect::<Vec<_>>();
        let body = json!({
            "session_id": response.conversation.session_id,
            "thread_id": response.conversation.thread_id,
            "content": response.content,
            "parts": response.parts,
            "resolved_parts": resolved_parts,
            "artifacts": response.artifacts,
            "artifact_attachments": artifact_attachments,
            "metadata": response.metadata,
        });
        let audit =
            OutputAuditSpan::start(self.observer.clone(), format!("http:{safe_target}"), &body)?;
        let delivery = async {
            let result = request
                .json(&body)
                .send()
                .await
                .with_context(|| format!("failed to deliver HTTP output to {safe_target}"))?;
            let status = result.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after_ms = http_retry_after_ms(result.headers()).unwrap_or(1_000);
                return Err(retry_after_delivery_error(
                    retry_after_ms,
                    "http output target returned 429",
                ));
            }
            if !status.is_success() {
                let message = format!("http output target returned HTTP {}", status.as_u16());
                if status.is_client_error() {
                    return Err(terminal_delivery_error(message));
                }
                return Err(anyhow!(message));
            }
            Ok::<_, anyhow::Error>(status)
        }
        .await;
        match delivery {
            Ok(status) => {
                audit.record_success(&json!({ "status": status.as_u16() }), "delivered")?;
                Ok(())
            }
            Err(error) => {
                audit.record_failure(&error)?;
                Err(error)
            }
        }
    }
}

impl HttpOutputPlugin {
    async fn client_for_route(
        &self,
        route: &crate::connectors::config::HttpReplyRoute,
    ) -> Result<reqwest::Client> {
        let parsed = reqwest::Url::parse(&route.url).context("invalid http reply target URL")?;
        let Some(host) = parsed.host_str() else {
            bail!("http reply target must include a host");
        };
        if parse_http_reply_host_ip(host).is_some() {
            return Ok(self.client.clone());
        }
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| anyhow!("http reply target must include a resolvable port"))?;
        let addrs = lookup_host((host, port))
            .await
            .with_context(|| format!("failed to resolve http reply target host {host}"))?
            .collect::<Vec<_>>();
        if addrs.is_empty() {
            bail!("http reply target host {host} did not resolve to any address");
        }
        if !route.allow_private_network {
            for addr in &addrs {
                ensure_public_http_reply_ip(addr.ip())?;
            }
        }
        Ok(output_http_client_with_resolved_host(host, &addrs))
    }
}

fn render_http_part(part: &ContentPart) -> HttpOutputPart<'_> {
    match part {
        ContentPart::Text { text } => HttpOutputPart::Text { text },
        ContentPart::Attachment { attachment } => HttpOutputPart::Attachment {
            attachment: http_attachment_descriptor(attachment),
        },
    }
}

fn http_attachment_descriptor(attachment: &AttachmentRef) -> HttpAttachmentDescriptor<'_> {
    HttpAttachmentDescriptor {
        id: &attachment.id,
        media_type: &attachment.media_type,
        file_name: attachment.file_name.as_deref(),
        sha256: attachment.sha256.as_deref(),
        byte_length: attachment.byte_length,
        download_path: format!("/v1/assets/{}/raw", attachment.id),
    }
}

fn http_delivery_id(response: &ResponseEnvelope) -> Option<&str> {
    response
        .metadata
        .get("delivery_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn http_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
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
impl DeliveryTransport for HttpOutputPlugin {
    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        <Self as OutputPlugin>::deliver(self, response).await
    }
}

#[cfg(test)]
mod tests {
    use kheish_types::{ConversationKey, ReplyHandle};
    use serde_json::json;

    use super::*;

    #[test]
    fn http_output_retry_after_parses_seconds_header() {
        assert_eq!(parse_retry_after_header_ms("3"), Some(3000));
        assert_eq!(parse_retry_after_header_ms(" 1 "), Some(1000));
        assert_eq!(parse_retry_after_header_ms("999999"), Some(3_600_000));
        assert_eq!(parse_retry_after_header_ms("not-seconds"), None);
    }

    #[test]
    fn http_output_429_without_retry_after_is_retryable() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(http_retry_after_ms(&headers).unwrap_or(1_000), 1_000);
    }

    #[test]
    fn http_output_retry_after_parses_http_date_header() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(60);
        let header = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let retry_after =
            parse_retry_after_header_ms(&header).expect("HTTP-date Retry-After should parse");
        assert!(
            (1..=60_000).contains(&retry_after),
            "unexpected retry_after: {retry_after}"
        );
    }

    #[test]
    fn http_output_delivery_id_uses_stable_delivery_context() {
        let response = ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "s1".to_string(),
                thread_id: None,
            },
            reply_targets: Vec::new(),
            reply: Some(ReplyHandle {
                plugin: "http".to_string(),
                address: "https://example.com/callback".to_string(),
            }),
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: json!({ "delivery_id": "delivery-123" }),
        };
        assert_eq!(http_delivery_id(&response), Some("delivery-123"));
    }
}
