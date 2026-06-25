use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::bash::{BashTool, configure_bash_command_workdir};
use crate::editing::EditFileTool;
use crate::files::{ReadFileTool, WriteFileTool};
use crate::patching::ApplyPatchTool;
use crate::search::{GrepSearchTool, ListFilesTool};
use crate::shared::{CodingToolConfig, SharedConfig};
use crate::web::{
    ProviderWebSearchService, WebFetchDnsResolver, WebFetchTool, WebSearchBackendOutput,
    WebSearchHit, WebSearchRequest, WebSearchRoute, WebSearchTool, filter_search_hits_by_domain,
    parse_duckduckgo_results,
};
use kheish_runtime::{SandboxProfile, Tool, ToolContext};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct StubProviderWebSearchService;

struct StaticDnsResolver {
    records: BTreeMap<String, Vec<SocketAddr>>,
    calls: Arc<Mutex<Vec<(String, u16)>>>,
}

impl StaticDnsResolver {
    fn new(records: impl IntoIterator<Item = (String, Vec<SocketAddr>)>) -> Self {
        Self {
            records: records.into_iter().collect(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl WebFetchDnsResolver for StaticDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        self.calls.lock().await.push((host.to_string(), port));
        Ok(self.records.get(host).cloned().unwrap_or_else(Vec::new))
    }
}

fn tool_context(call_id: &str, sandbox: SandboxProfile) -> ToolContext {
    ToolContext {
        call_id: call_id.to_string(),
        sandbox,
        metadata: Value::Null,
    }
}

#[async_trait]
impl ProviderWebSearchService for StubProviderWebSearchService {
    async fn search(
        &self,
        route: &WebSearchRoute,
        request: &WebSearchRequest,
    ) -> Result<Option<WebSearchBackendOutput>> {
        if route.provider != "openai" {
            return Ok(None);
        }
        Ok(Some(WebSearchBackendOutput {
            engine: "openai_web_search".to_string(),
            implementation: "provider_native".to_string(),
            provider: Some(route.provider.clone()),
            model: route.model.clone(),
            results: vec![WebSearchHit::new(
                format!("native: {}", request.query),
                "https://example.com/native",
                "provider snippet",
            )],
        }))
    }
}

#[tokio::test]
async fn default_tools_can_edit_and_search_workspace_files() -> Result<()> {
    let workspace = tempdir()?;
    let file = workspace.path().join("src/example.txt");
    fs::create_dir_all(file.parent().expect("parent")).await?;
    fs::write(&file, "alpha\nbeta\ngamma\n").await?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let read = ReadFileTool::new(shared.clone())
        .execute(
            ToolContext {
                call_id: "read".to_string(),
                sandbox: SandboxProfile::ReadOnly,
                metadata: Value::Null,
            },
            json!({"path": "src/example.txt", "start_line": 2, "line_count": 1}),
        )
        .await?;
    assert_eq!(read.output["content"], "beta");

    EditFileTool::new(shared.clone())
        .execute(
            ToolContext {
                call_id: "edit".to_string(),
                sandbox: SandboxProfile::WorkspaceWrite,
                metadata: Value::Null,
            },
            json!({"path": "src/example.txt", "old_text": "beta", "new_text": "delta"}),
        )
        .await?;

    let grep = GrepSearchTool::new(shared.clone())
        .execute(
            ToolContext {
                call_id: "grep".to_string(),
                sandbox: SandboxProfile::ReadOnly,
                metadata: Value::Null,
            },
            json!({"pattern": "delta"}),
        )
        .await?;
    assert_eq!(grep.output["matches"].as_array().map(Vec::len), Some(1));

    let listed = ListFilesTool::new(shared)
        .execute(
            ToolContext {
                call_id: "list".to_string(),
                sandbox: SandboxProfile::ReadOnly,
                metadata: Value::Null,
            },
            json!({"base": "src"}),
        )
        .await?;
    assert!(
        listed.output["paths"]
            .as_array()
            .expect("paths")
            .iter()
            .any(|path| path == "example.txt")
    );
    Ok(())
}

#[tokio::test]
async fn bash_and_web_fetch_tools_return_outputs() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));

    let bash = BashTool::new(shared.clone())
        .execute(
            ToolContext {
                call_id: "bash".to_string(),
                sandbox: SandboxProfile::WorkspaceWrite,
                metadata: Value::Null,
            },
            json!({"command": "printf 'hello'"}),
        )
        .await?;
    assert_eq!(bash.output["stdout"], "hello");

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = socket.read(&mut buffer).await?;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nworld")
            .await?;
        Result::<()>::Ok(())
    });

    let fetch = WebFetchTool::new(shared)
        .execute(
            ToolContext {
                call_id: "fetch".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://{address}")}),
        )
        .await?;
    server.await??;
    assert_eq!(fetch.output["status"], 200);
    assert_eq!(fetch.output["body"], "world");
    Ok(())
}

#[tokio::test]
async fn web_fetch_blocks_private_hosts_by_default() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let error = WebFetchTool::new(shared.clone())
        .execute(
            ToolContext {
                call_id: "fetch-private".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: Value::Null,
            },
            json!({"url": format!("http://{address}")}),
        )
        .await
        .expect_err("web_fetch should reject loopback by default");
    assert!(error.to_string().contains("private network"));

    for blocked in [
        "http://localhost/",
        "http://0.0.0.0/",
        "http://10.0.0.1/",
        "http://172.16.0.1/",
        "http://192.168.0.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://user:pass@example.com/",
    ] {
        let error = WebFetchTool::new(shared.clone())
            .execute(
                ToolContext {
                    call_id: "fetch-blocked".to_string(),
                    sandbox: SandboxProfile::NetworkEnabled,
                    metadata: Value::Null,
                },
                json!({"url": blocked}),
            )
            .await
            .expect_err("web_fetch should reject unsafe target before connecting");
        let message = error.to_string();
        assert!(
            message.contains("private network") || message.contains("credentials"),
            "unexpected error for {blocked}: {message}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn web_fetch_pins_hostname_to_validated_addrs_and_preserves_host_header() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let resolver = Arc::new(StaticDnsResolver::new([(
        "rebind.test".to_string(),
        vec![address],
    )]));
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0; 2048];
        let read = socket.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let lower_request = request.to_ascii_lowercase();
        assert!(
            lower_request.contains(&format!("host: rebind.test:{}", address.port())),
            "web_fetch should keep the original host header while using the pinned address: {request}"
        );
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\nConnection: close\r\n\r\npinned",
            )
            .await?;
        Result::<()>::Ok(())
    });

    let fetch = WebFetchTool::with_dns_resolver(shared, resolver)
        .execute(
            ToolContext {
                call_id: "fetch-pinned".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://rebind.test:{}/target", address.port())}),
        )
        .await?;
    server.await??;
    assert_eq!(fetch.output["body"], "pinned");
    Ok(())
}

#[tokio::test]
async fn web_fetch_blocks_hostname_resolving_to_private_by_default() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let resolver = Arc::new(StaticDnsResolver::new([(
        "public-looking.test".to_string(),
        vec![address],
    )]));

    let error = WebFetchTool::with_dns_resolver(shared, resolver)
        .execute(
            ToolContext {
                call_id: "fetch-private-dns".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: Value::Null,
            },
            json!({"url": format!("http://public-looking.test:{}/secret", address.port())}),
        )
        .await
        .expect_err("web_fetch should reject hostnames resolving to private addresses");
    assert!(error.to_string().contains("private network"));
    let contacted = tokio::time::timeout(Duration::from_millis(100), listener.accept())
        .await
        .is_ok();
    assert!(!contacted, "private-resolving host should not be contacted");
    Ok(())
}

#[tokio::test]
async fn web_fetch_rejects_mixed_public_private_dns_answers() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let resolver = Arc::new(StaticDnsResolver::new([(
        "mixed.test".to_string(),
        vec![
            "93.184.216.34:80".parse::<SocketAddr>()?,
            "127.0.0.1:80".parse::<SocketAddr>()?,
        ],
    )]));

    let error = WebFetchTool::with_dns_resolver(shared, resolver)
        .execute(
            ToolContext {
                call_id: "fetch-mixed-dns".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: Value::Null,
            },
            json!({"url": "http://mixed.test/"}),
        )
        .await
        .expect_err("web_fetch should reject any DNS answer set containing private addresses");
    assert!(error.to_string().contains("private network"));
    Ok(())
}

#[tokio::test]
async fn web_fetch_revalidates_and_pins_each_redirect_target() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let resolver = Arc::new(StaticDnsResolver::new([
        ("start.test".to_string(), vec![address]),
        ("final.test".to_string(), vec![address]),
    ]));
    let calls = resolver.calls.clone();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = first.read(&mut buffer).await?;
        first
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://final.test:{}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await?;
        drop(first);

        let (mut second, _) = listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = second.read(&mut buffer).await?;
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfinal",
            )
            .await?;
        Result::<()>::Ok(())
    });

    let fetch = WebFetchTool::with_dns_resolver(shared, resolver)
        .execute(
            ToolContext {
                call_id: "fetch-redirect-pinned".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://start.test:{}/start", address.port())}),
        )
        .await?;
    server.await??;
    assert_eq!(fetch.output["redirects"], 1);
    assert_eq!(
        fetch.output["final_url"],
        format!("http://final.test:{}/final", address.port())
    );
    assert_eq!(
        *calls.lock().await,
        vec![
            ("start.test".to_string(), address.port()),
            ("final.test".to_string(), address.port()),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn web_fetch_rejects_non_text_and_oversized_responses() -> Result<()> {
    let workspace = tempdir()?;
    let mut config = CodingToolConfig::new(workspace.path());
    config.max_read_bytes = 8;
    let shared = Arc::new(SharedConfig::new(config));

    let binary_listener = TcpListener::bind("127.0.0.1:0").await?;
    let binary_address = binary_listener.local_addr()?;
    let binary_server = tokio::spawn(async move {
        let (mut socket, _) = binary_listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = socket.read(&mut buffer).await?;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\n\r\n1234",
            )
            .await?;
        Result::<()>::Ok(())
    });
    let binary_error = WebFetchTool::new(shared.clone())
        .execute(
            ToolContext {
                call_id: "fetch-binary".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://{binary_address}")}),
        )
        .await
        .expect_err("web_fetch should reject non-text content");
    binary_server.await??;
    assert!(binary_error.to_string().contains("non-text content type"));

    let huge_listener = TcpListener::bind("127.0.0.1:0").await?;
    let huge_address = huge_listener.local_addr()?;
    let huge_server = tokio::spawn(async move {
        let (mut socket, _) = huge_listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = socket.read(&mut buffer).await?;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 64\r\n\r\n")
            .await?;
        let _ = socket.write_all(&vec![b'a'; 64]).await;
        Result::<()>::Ok(())
    });
    let huge_error = WebFetchTool::new(shared)
        .execute(
            ToolContext {
                call_id: "fetch-huge".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://{huge_address}")}),
        )
        .await
        .expect_err("web_fetch should reject declared oversized responses");
    huge_server.await??;
    assert!(huge_error.to_string().contains("larger than 8 bytes"));
    Ok(())
}

#[tokio::test]
async fn web_fetch_follows_manual_redirects_with_final_url() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = first.read(&mut buffer).await?;
        first
            .write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        drop(first);

        let (mut second, _) = listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = second.read(&mut buffer).await?;
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfinal",
            )
            .await?;
        Result::<()>::Ok(())
    });

    let fetch = WebFetchTool::new(shared)
        .execute(
            ToolContext {
                call_id: "fetch-redirect".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://{address}/start")}),
        )
        .await?;
    server.await??;
    assert_eq!(fetch.output["redirects"], 1);
    assert_eq!(fetch.output["final_url"], format!("http://{address}/final"));
    assert_eq!(fetch.output["body"], "final");
    Ok(())
}

#[tokio::test]
async fn web_fetch_stream_limits_chunked_huge_pages() -> Result<()> {
    let workspace = tempdir()?;
    let mut config = CodingToolConfig::new(workspace.path());
    config.max_read_bytes = 8;
    let shared = Arc::new(SharedConfig::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0; 1024];
        let _ = socket.read(&mut buffer).await?;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n")
            .await?;
        let _ = socket.write_all(&vec![b'x'; 64]).await;
        Result::<()>::Ok(())
    });

    let fetch = WebFetchTool::new(shared)
        .execute(
            ToolContext {
                call_id: "fetch-stream-cap".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({"allow_private_network": true}),
            },
            json!({"url": format!("http://{address}")}),
        )
        .await?;
    server.await??;
    assert_eq!(fetch.output["bytes_read"], 8);
    assert_eq!(fetch.output["truncated"], true);
    assert_eq!(fetch.output["body"], "xxxxxxxx");
    Ok(())
}

#[test]
fn duckduckgo_result_parser_extracts_hits_and_redirects() {
    let html = r#"
    <div class="result">
      <h2 class="result__title">
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs">Example Docs</a>
      </h2>
      <a class="result__snippet">Current documentation for the example project.</a>
    </div>
    <div class="result">
      <h2 class="result__title">
        <a class="result__a" href="https://blog.example.org/post">Example Blog</a>
      </h2>
      <div class="result__snippet">An additional reference.</div>
    </div>
    "#;

    let hits = parse_duckduckgo_results(html);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].title, "Example Docs");
    assert_eq!(hits[0].url, "https://example.com/docs");
    assert_eq!(hits[0].domain, "example.com");
    assert_eq!(hits[1].url, "https://blog.example.org/post");
    assert_eq!(hits[1].snippet, "An additional reference.");
}

#[test]
fn duckduckgo_result_parser_drops_localhost_hits() {
    let html = r#"
    <div class="result">
      <h2 class="result__title">
        <a class="result__a" href="http://127.0.0.1/admin">Loopback</a>
      </h2>
      <div class="result__snippet">Should not be returned.</div>
    </div>
    <div class="result">
      <h2 class="result__title">
        <a class="result__a" href="https://example.com/docs">Example</a>
      </h2>
      <div class="result__snippet">A public result.</div>
    </div>
    "#;

    let hits = parse_duckduckgo_results(html);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://example.com/docs");
}

#[test]
fn web_search_domain_filters_match_subdomains() {
    let hits = vec![
        WebSearchHit {
            title: "A".to_string(),
            url: "https://docs.example.com/a".to_string(),
            snippet: String::new(),
            domain: "docs.example.com".to_string(),
        },
        WebSearchHit {
            title: "B".to_string(),
            url: "https://elsewhere.test/b".to_string(),
            snippet: String::new(),
            domain: "elsewhere.test".to_string(),
        },
    ];

    let filtered = filter_search_hits_by_domain(hits.clone(), &["example.com".to_string()], &[]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].domain, "docs.example.com");

    let filtered = filter_search_hits_by_domain(hits, &[], &["example.com".to_string()]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].domain, "elsewhere.test");
}

#[test]
fn web_search_domain_filters_allow_and_block_together() {
    let hits = vec![
        WebSearchHit {
            title: "Allowed".to_string(),
            url: "https://docs.example.com/a".to_string(),
            snippet: String::new(),
            domain: "docs.example.com".to_string(),
        },
        WebSearchHit {
            title: "Blocked".to_string(),
            url: "https://blocked.example.com/b".to_string(),
            snippet: String::new(),
            domain: "blocked.example.com".to_string(),
        },
        WebSearchHit {
            title: "Elsewhere".to_string(),
            url: "https://elsewhere.test/c".to_string(),
            snippet: String::new(),
            domain: "elsewhere.test".to_string(),
        },
    ];

    let filtered = filter_search_hits_by_domain(
        hits,
        &["example.com".to_string()],
        &["blocked.example.com".to_string()],
    );
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].domain, "docs.example.com");
}

#[tokio::test]
async fn web_search_sources_markdown_escapes_titles_and_exposes_citations() -> Result<()> {
    struct WeirdProviderWebSearchService;

    #[async_trait]
    impl ProviderWebSearchService for WeirdProviderWebSearchService {
        async fn search(
            &self,
            _route: &WebSearchRoute,
            _request: &WebSearchRequest,
        ) -> Result<Option<WebSearchBackendOutput>> {
            Ok(Some(WebSearchBackendOutput {
                engine: "weird".to_string(),
                implementation: "provider_native".to_string(),
                provider: Some("openai".to_string()),
                model: Some("test-model".to_string()),
                results: vec![WebSearchHit::new(
                    "Bad ](title\nnext",
                    "https://example.com/a(b)c",
                    "snippet",
                )],
            }))
        }
    }

    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let tool =
        WebSearchTool::with_provider_search(shared, Some(Arc::new(WeirdProviderWebSearchService)));
    let output = tool
        .execute(
            ToolContext {
                call_id: "web-search-weird".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({
                    "session_id": "session-1",
                    "provider": "openai",
                    "model": "test-model",
                }),
            },
            json!({"query": "markdown escaping"}),
        )
        .await?;
    assert_eq!(
        output.output["sources_markdown"],
        "- [Bad \\]\\(title next](https://example.com/a%28b%29c)"
    );
    assert_eq!(output.output["citations"][0]["domain"], "example.com");
    Ok(())
}

#[tokio::test]
async fn web_search_filters_provider_native_private_results() -> Result<()> {
    struct PrivateProviderWebSearchService;

    #[async_trait]
    impl ProviderWebSearchService for PrivateProviderWebSearchService {
        async fn search(
            &self,
            _route: &WebSearchRoute,
            _request: &WebSearchRequest,
        ) -> Result<Option<WebSearchBackendOutput>> {
            Ok(Some(WebSearchBackendOutput {
                engine: "private-fixture".to_string(),
                implementation: "provider_native".to_string(),
                provider: Some("openai".to_string()),
                model: Some("test-model".to_string()),
                results: vec![
                    WebSearchHit::new("Loopback", "http://127.0.0.1/admin", "private"),
                    WebSearchHit::new("Example", "https://example.com/docs", "public"),
                ],
            }))
        }
    }

    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let tool = WebSearchTool::with_provider_search(
        shared,
        Some(Arc::new(PrivateProviderWebSearchService)),
    );
    let output = tool
        .execute(
            ToolContext {
                call_id: "web-search-private".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({
                    "session_id": "session-1",
                    "provider": "openai",
                    "model": "test-model",
                }),
            },
            json!({"query": "private filtering"}),
        )
        .await?;

    assert_eq!(output.output["returned_results"], 1);
    assert_eq!(
        output.output["results"][0]["url"],
        "https://example.com/docs"
    );
    Ok(())
}

#[tokio::test]
async fn tools_honor_workspace_root_overrides_from_context() -> Result<()> {
    let workspace = tempdir()?;
    let child_root = workspace.path().join("child");
    fs::create_dir_all(&child_root).await?;
    fs::write(child_root.join("note.txt"), "child-only").await?;
    fs::write(workspace.path().join("root.txt"), "parent").await?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let ctx = ToolContext {
        call_id: "child-read".to_string(),
        sandbox: SandboxProfile::ReadOnly,
        metadata: json!({
            "workspace_root": child_root.display().to_string(),
        }),
    };

    let read = ReadFileTool::new(shared.clone())
        .execute(ctx.clone(), json!({"path": "note.txt"}))
        .await?;
    assert_eq!(read.output["content"], "child-only");

    let err = ReadFileTool::new(shared)
        .execute(ctx, json!({"path": "../root.txt"}))
        .await
        .expect_err("reads should stay inside the override root");
    assert!(err.to_string().contains("escapes workspace root"));
    Ok(())
}

#[tokio::test]
async fn web_search_prefers_provider_native_backends_when_the_route_supports_them() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let tool =
        WebSearchTool::with_provider_search(shared, Some(Arc::new(StubProviderWebSearchService)));
    let output = tool
        .execute(
            ToolContext {
                call_id: "web-search".to_string(),
                sandbox: SandboxProfile::NetworkEnabled,
                metadata: json!({
                    "session_id": "session-1",
                    "provider": "openai",
                    "model": "gpt-5.4",
                }),
            },
            json!({"query": "sqlite wal"}),
        )
        .await?;
    assert_eq!(output.output["implementation"], "provider_native");
    assert_eq!(output.output["provider"], "openai");
    assert_eq!(output.output["engine"], "openai_web_search");
    assert_eq!(
        output.output["results"][0]["url"],
        Value::String("https://example.com/native".to_string())
    );
    Ok(())
}

#[tokio::test]
async fn read_file_binary_fixture_returns_metadata_and_base64() -> Result<()> {
    let workspace = tempdir()?;
    let file = workspace.path().join("binary.bin");
    fs::write(&file, [0xff, 0x00, 0x41]).await?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let read = ReadFileTool::new(shared)
        .execute(
            tool_context("read-binary", SandboxProfile::ReadOnly),
            json!({"path": "binary.bin", "include_base64": true}),
        )
        .await?;
    assert_eq!(read.output["binary"], true);
    assert_eq!(read.output["encoding"], "binary");
    assert_eq!(read.output["content"], "");
    assert_eq!(read.output["bytes_read"], 3);
    assert_eq!(
        read.output["content_base64"],
        BASE64.encode([0xff, 0x00, 0x41])
    );
    assert!(read.output["sha256"].as_str().is_some());
    Ok(())
}

#[tokio::test]
async fn write_file_supports_base64_and_expected_sha256() -> Result<()> {
    let workspace = tempdir()?;
    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));

    let initial = WriteFileTool::new(shared.clone())
        .execute(
            tool_context("write-binary", SandboxProfile::WorkspaceWrite),
            json!({
                "path": "binary.bin",
                "content_base64": BASE64.encode([0x00, 0x01, 0xfe]),
            }),
        )
        .await?;
    assert_eq!(
        fs::read(workspace.path().join("binary.bin")).await?,
        [0x00, 0x01, 0xfe]
    );

    let stale = WriteFileTool::new(shared)
        .execute(
            tool_context("write-stale", SandboxProfile::WorkspaceWrite),
            json!({
                "path": "binary.bin",
                "content": "new",
                "expected_sha256": "not-the-current-digest",
            }),
        )
        .await
        .expect_err("stale expected_sha256 should reject the write");
    assert!(stale.to_string().contains("expected_sha256 mismatch"));
    assert_eq!(
        fs::read(workspace.path().join("binary.bin")).await?,
        [0x00, 0x01, 0xfe]
    );
    assert_eq!(initial.output["bytes_written"], 3);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn path_locks_span_shared_config_instances() -> Result<()> {
    let workspace = tempdir()?;
    let path = workspace.path().join("locked.txt");
    fs::write(&path, "initial").await?;

    let shared_a = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let shared_b = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));

    let guard = shared_a.lock_path(&path).await?;
    let contender = tokio::spawn({
        let shared_b = shared_b.clone();
        let path = path.clone();
        async move {
            let _guard = shared_b.lock_path(&path).await?;
            Ok::<(), anyhow::Error>(())
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !contender.is_finished(),
        "independent SharedConfig instances should coordinate through the process file lock"
    );

    drop(guard);
    tokio::time::timeout(Duration::from_secs(2), contender)
        .await
        .expect("contender should acquire the lock after release")??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn write_and_edit_reject_existing_symlink_escape() -> Result<()> {
    let workspace = tempdir()?;
    let outside = tempdir()?;
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, "outside").await?;
    std::os::unix::fs::symlink(&outside_file, workspace.path().join("link.txt"))?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let write_error = WriteFileTool::new(shared.clone())
        .execute(
            tool_context("write-link", SandboxProfile::WorkspaceWrite),
            json!({"path": "link.txt", "content": "escape"}),
        )
        .await
        .expect_err("write_file should reject a final symlink");
    assert!(
        write_error
            .to_string()
            .contains("refusing to modify symlink")
    );

    let edit_error = EditFileTool::new(shared)
        .execute(
            tool_context("edit-link", SandboxProfile::WorkspaceWrite),
            json!({"path": "link.txt", "old_text": "outside", "new_text": "escape"}),
        )
        .await
        .expect_err("edit_file should reject a final symlink");
    assert!(
        edit_error
            .to_string()
            .contains("refusing to modify symlink")
    );
    assert_eq!(fs::read_to_string(&outside_file).await?, "outside");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn read_and_write_reject_parent_symlink_escape() -> Result<()> {
    let workspace = tempdir()?;
    let outside = tempdir()?;
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, "outside secret").await?;
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked"))?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let read_error = ReadFileTool::new(shared.clone())
        .execute(
            tool_context("read-parent-link", SandboxProfile::ReadOnly),
            json!({"path": "linked/secret.txt"}),
        )
        .await
        .expect_err("read_file should reject a parent symlink escape");
    assert!(read_error.to_string().contains("escapes workspace root"));

    let write_error = WriteFileTool::new(shared)
        .execute(
            tool_context("write-parent-link", SandboxProfile::WorkspaceWrite),
            json!({"path": "linked/secret.txt", "content": "changed"}),
        )
        .await
        .expect_err("write_file should reject a parent symlink escape");
    assert!(write_error.to_string().contains("escapes workspace root"));
    assert_eq!(fs::read_to_string(&outside_file).await?, "outside secret");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn bash_workdir_uses_validated_directory_fd_after_path_swap() -> Result<()> {
    let workspace = tempdir()?;
    let outside = tempdir()?;
    let workdir = workspace.path().join("work");
    let renamed_workdir = workspace.path().join("work-renamed");
    fs::create_dir(&workdir).await?;
    fs::write(workdir.join("marker.txt"), "inside-marker").await?;
    fs::write(outside.path().join("marker.txt"), "outside-marker").await?;

    let config = CodingToolConfig::new(workspace.path());
    let mut command = Command::new(&config.shell);
    command.arg("-lc").arg("cat marker.txt");
    let (_resolved, _guard) = configure_bash_command_workdir(
        &config,
        &tool_context("bash-workdir-swap", SandboxProfile::WorkspaceWrite),
        &mut command,
        Some("work"),
    )?;

    fs::rename(&workdir, &renamed_workdir).await?;
    std::os::unix::fs::symlink(outside.path(), &workdir)?;

    let output = command.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bash should still start from the opened directory fd: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(stdout.contains("inside-marker"));
    assert!(!stdout.contains("outside-marker"));
    Ok(())
}

#[tokio::test]
async fn apply_patch_applies_hunks_and_rejects_partial_failure() -> Result<()> {
    let workspace = tempdir()?;
    fs::write(workspace.path().join("a.txt"), "alpha\nbeta\n").await?;
    fs::write(workspace.path().join("b.txt"), "gamma\n").await?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let patched = ApplyPatchTool::new(shared.clone())
        .execute(
            tool_context("patch-ok", SandboxProfile::WorkspaceWrite),
            json!({
                "hunks": [
                    {"path": "a.txt", "old_text": "alpha", "new_text": "ALPHA"},
                    {"path": "a.txt", "old_text": "beta", "new_text": "BETA"}
                ]
            }),
        )
        .await?;
    assert_eq!(
        fs::read_to_string(workspace.path().join("a.txt")).await?,
        "ALPHA\nBETA\n"
    );
    assert_eq!(patched.output["file_count"], 1);

    let failed = ApplyPatchTool::new(shared)
        .execute(
            tool_context("patch-fail", SandboxProfile::WorkspaceWrite),
            json!({
                "hunks": [
                    {"path": "a.txt", "old_text": "ALPHA", "new_text": "alpha"},
                    {"path": "b.txt", "old_text": "missing", "new_text": "delta"}
                ]
            }),
        )
        .await
        .expect_err("all hunks should validate before any write");
    assert!(failed.to_string().contains("patch hunk target not found"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("a.txt")).await?,
        "ALPHA\nBETA\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("b.txt")).await?,
        "gamma\n"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_rolls_back_prior_files_when_later_write_fails() -> Result<()> {
    let workspace = tempdir()?;
    fs::write(workspace.path().join("a.txt"), "alpha\n").await?;
    let readonly_dir = workspace.path().join("readonly");
    fs::create_dir(&readonly_dir).await?;
    fs::write(readonly_dir.join("b.txt"), "gamma\n").await?;
    fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o500)).await?;

    let shared = Arc::new(SharedConfig::new(CodingToolConfig::new(workspace.path())));
    let result = ApplyPatchTool::new(shared)
        .execute(
            tool_context("patch-write-fail", SandboxProfile::WorkspaceWrite),
            json!({
                "hunks": [
                    {"path": "a.txt", "old_text": "alpha", "new_text": "ALPHA"},
                    {"path": "readonly/b.txt", "old_text": "gamma", "new_text": "delta"}
                ]
            }),
        )
        .await;

    fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o700)).await?;
    let error = result.expect_err("second write should fail and roll back first file");
    assert!(
        error.to_string().contains("failed to write")
            || error.to_string().contains("failed to stage write")
            || error
                .to_string()
                .contains("failed to create temporary file"),
        "unexpected apply_patch write failure: {error:#}"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("a.txt")).await?,
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(readonly_dir.join("b.txt")).await?,
        "gamma\n"
    );
    Ok(())
}
