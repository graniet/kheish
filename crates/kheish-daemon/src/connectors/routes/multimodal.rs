use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Client, Url};

use crate::InlineAssetUpload;
use crate::SubmitInputItemRequest;
use crate::SubmitInputRequest;
use crate::assets::MAX_ASSET_BYTES;

const MAX_CONNECTOR_MEDIA_ITEMS: usize = 8;
const CONNECTOR_MEDIA_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);

static CONNECTOR_MEDIA_CLIENT: OnceLock<Client> = OnceLock::new();

/// One externally hosted file referenced by an ingress connector event.
#[derive(Clone, Debug)]
pub(crate) struct ConnectorMediaRef {
    /// Stable human-readable file name used for display and MIME inference.
    pub file_name: String,
    /// Optional caller-declared MIME type forwarded to daemon asset validation.
    pub media_type: Option<String>,
    /// Optional size hint advertised by the upstream connector.
    pub byte_length_hint: Option<u64>,
    /// The absolute HTTPS or HTTP URL used to fetch the raw file payload.
    pub url: String,
    /// Optional bearer token attached to the download request.
    pub bearer_token: Option<String>,
}

/// Materializes one connector-originated text body plus inline uploads into the canonical
/// `SubmitInputRequest` fields accepted by the daemon.
pub(crate) fn apply_connector_multimodal_input(
    request: &mut SubmitInputRequest,
    text: Option<String>,
    uploads: Vec<InlineAssetUpload>,
) -> Result<()> {
    if uploads.len() > MAX_CONNECTOR_MEDIA_ITEMS {
        bail!(
            "connector input exceeds the {} attachment limit",
            MAX_CONNECTOR_MEDIA_ITEMS
        );
    }
    let text = text.unwrap_or_default();
    if uploads.is_empty() {
        if text.trim().is_empty() {
            bail!("missing connector content");
        }
        request.content = text;
        request.input_items.clear();
        request.attachments.clear();
        return Ok(());
    }

    request.content.clear();
    request.attachments.clear();
    request.input_items = Vec::with_capacity(uploads.len() + usize::from(!text.trim().is_empty()));
    if !text.trim().is_empty() {
        request
            .input_items
            .push(SubmitInputItemRequest::Text { text: text.clone() });
    }
    request
        .input_items
        .extend(uploads.into_iter().map(SubmitInputItemRequest::InlineAsset));
    Ok(())
}

/// Rejects connector submissions that do not contribute any user-visible text or attachments.
pub(crate) fn ensure_connector_request_not_empty(request: &SubmitInputRequest) -> Result<()> {
    let has_text = !request.content.trim().is_empty()
        || request.input_items.iter().any(|item| match item {
            SubmitInputItemRequest::Text { text } => !text.trim().is_empty(),
            SubmitInputItemRequest::AssetReference { .. }
            | SubmitInputItemRequest::BoardReference { .. }
            | SubmitInputItemRequest::InlineAsset(_) => true,
        });
    let has_attachments = !request.attachments.is_empty();
    if has_text || has_attachments {
        Ok(())
    } else {
        bail!("missing connector content");
    }
}

/// Downloads external connector media into canonical inline uploads using strict size and timeout
/// bounds before the daemon imports the files into its asset store.
pub(crate) async fn download_connector_media(
    media: &[ConnectorMediaRef],
) -> Result<Vec<InlineAssetUpload>> {
    if media.len() > MAX_CONNECTOR_MEDIA_ITEMS {
        bail!(
            "connector input exceeds the {} attachment limit",
            MAX_CONNECTOR_MEDIA_ITEMS
        );
    }
    let client = connector_ingress_http_client();
    let mut uploads = Vec::with_capacity(media.len());
    for item in media {
        if let Some(byte_length_hint) = item.byte_length_hint
            && byte_length_hint > MAX_ASSET_BYTES as u64
        {
            bail!(
                "connector file '{}' exceeds the {} byte limit",
                item.file_name,
                MAX_ASSET_BYTES
            );
        }
        uploads.push(download_one_media(client, item).await?);
    }
    Ok(uploads)
}

/// Returns the shared HTTP client used by ingress connectors for bounded media fetches.
pub(super) fn connector_ingress_http_client() -> &'static Client {
    CONNECTOR_MEDIA_CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent("kheish-daemon/connectors-ingress")
            .timeout(CONNECTOR_MEDIA_DOWNLOAD_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("connector media client should build")
    })
}

async fn download_one_media(
    client: &Client,
    item: &ConnectorMediaRef,
) -> Result<InlineAssetUpload> {
    let url = Url::parse(&item.url)
        .with_context(|| format!("invalid connector file url '{}'", item.url))?;
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported connector file url scheme '{other}'"),
    }

    let mut request = client.get(url);
    if let Some(bearer_token) = item.bearer_token.as_deref() {
        request = request.header(AUTHORIZATION, format!("Bearer {bearer_token}"));
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to fetch connector file '{}'", item.file_name))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to fetch connector file '{}'", item.file_name))?;
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && length > MAX_ASSET_BYTES
    {
        bail!(
            "connector file '{}' exceeds the {} byte limit",
            item.file_name,
            MAX_ASSET_BYTES
        );
    }

    let header_media_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(strip_media_type_parameters)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let bytes = collect_bounded_body(response).await?;
    let file_name = normalize_file_name(&item.file_name, &item.url)?;
    Ok(InlineAssetUpload {
        file_name,
        media_type: item.media_type.clone().or(header_media_type),
        content_base64: STANDARD.encode(bytes),
    })
}

async fn collect_bounded_body(response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while downloading connector media")?;
        if body.len().saturating_add(chunk.len()) > MAX_ASSET_BYTES {
            bail!("connector file exceeds the {} byte limit", MAX_ASSET_BYTES);
        }
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        bail!("connector file download returned an empty payload");
    }
    Ok(body)
}

fn normalize_file_name(file_name: &str, url: &str) -> Result<String> {
    let trimmed = file_name.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }
    let parsed = Url::parse(url).with_context(|| format!("invalid connector file url '{url}'"))?;
    let fallback = parsed
        .path_segments()
        .and_then(|segments| segments.last())
        .filter(|segment| !segment.trim().is_empty())
        .and_then(|segment| Path::new(segment).file_name())
        .and_then(|segment| segment.to_str())
        .filter(|segment| !segment.trim().is_empty())
        .ok_or_else(|| anyhow!("connector file is missing a file name"))?;
    Ok(fallback.to_string())
}

fn strip_media_type_parameters(value: &str) -> &str {
    value.split(';').next().map(str::trim).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectorMediaRef, apply_connector_multimodal_input, ensure_connector_request_not_empty,
        strip_media_type_parameters,
    };
    use crate::{SubmitInputItemRequest, SubmitInputRequest};

    fn blank_request() -> SubmitInputRequest {
        SubmitInputRequest {
            provider: None,
            source_plugin: Some("test".to_string()),
            source_kind: Some("unit".to_string()),
            actor_id: Some("tester".to_string()),
            content: String::new(),
            input_items: Vec::new(),
            attachments: Vec::new(),
            generation: None,
            completion_requirements: None,
            metadata: None,
            binding_keys: Vec::new(),
            reply_targets: Vec::new(),
            reply_plugin: None,
            reply_address: None,
        }
    }

    #[test]
    fn builds_legacy_text_only_connector_requests() {
        let mut request = blank_request();
        apply_connector_multimodal_input(&mut request, Some("hello".to_string()), Vec::new())
            .expect("text-only request should succeed");
        assert_eq!(request.content, "hello");
        assert!(request.input_items.is_empty());
        assert!(request.attachments.is_empty());
    }

    #[test]
    fn builds_rich_connector_requests_when_files_are_present() {
        let mut request = blank_request();
        apply_connector_multimodal_input(
            &mut request,
            Some("caption".to_string()),
            vec![crate::InlineAssetUpload {
                file_name: "image.png".to_string(),
                media_type: Some("image/png".to_string()),
                content_base64: "aGVsbG8=".to_string(),
            }],
        )
        .expect("multimodal request should succeed");
        assert!(request.content.is_empty());
        assert!(request.attachments.is_empty());
        assert_eq!(
            request.input_items,
            vec![
                SubmitInputItemRequest::Text {
                    text: "caption".to_string()
                },
                SubmitInputItemRequest::InlineAsset(crate::InlineAssetUpload {
                    file_name: "image.png".to_string(),
                    media_type: Some("image/png".to_string()),
                    content_base64: "aGVsbG8=".to_string(),
                })
            ]
        );
    }

    #[test]
    fn strips_content_type_parameters() {
        assert_eq!(
            strip_media_type_parameters("image/png; charset=utf-8"),
            "image/png"
        );
        assert_eq!(
            strip_media_type_parameters("application/json"),
            "application/json"
        );
    }

    #[test]
    fn connector_media_ref_keeps_caller_metadata() {
        let media = ConnectorMediaRef {
            file_name: "demo.pdf".to_string(),
            media_type: Some("application/pdf".to_string()),
            byte_length_hint: Some(128),
            url: "https://example.com/demo.pdf".to_string(),
            bearer_token: Some("secret".to_string()),
        };
        assert_eq!(media.file_name, "demo.pdf");
        assert_eq!(media.media_type.as_deref(), Some("application/pdf"));
        assert_eq!(media.byte_length_hint, Some(128));
        assert_eq!(media.bearer_token.as_deref(), Some("secret"));
    }

    #[test]
    fn rejects_empty_connector_request() {
        let request = blank_request();
        let error = ensure_connector_request_not_empty(&request)
            .expect_err("empty connector request should fail");
        assert_eq!(error.to_string(), "missing connector content");
    }

    #[test]
    fn accepts_asset_only_connector_request() {
        let mut request = blank_request();
        request.input_items = vec![SubmitInputItemRequest::AssetReference {
            asset_id: "asset-1".to_string(),
        }];
        ensure_connector_request_not_empty(&request)
            .expect("asset-only connector request should succeed");
    }
}
