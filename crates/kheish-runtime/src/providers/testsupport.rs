#![cfg(test)]

use parking_lot::Mutex;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawns a one-shot HTTP mock server that captures the request body.
pub(crate) async fn spawn_mock_server(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
    captured_request_body: Arc<Mutex<String>>,
) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let headers = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let body = body.to_string();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("server should accept");
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("request read should succeed");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request_text = String::from_utf8(request).expect("request must be utf-8");
        let content_length = request_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then_some(value)
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or_default();
        let body_start = request_text
            .find("\r\n\r\n")
            .map(|index| index + 4)
            .unwrap_or(request_text.len());
        let mut body_bytes = request_text.as_bytes()[body_start..].to_vec();
        while body_bytes.len() < content_length {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("body read should succeed");
            if read == 0 {
                break;
            }
            body_bytes.extend_from_slice(&buffer[..read]);
        }
        *captured_request_body.lock() = String::from_utf8(body_bytes).expect("body must be utf-8");

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("response write should succeed");
    });

    Ok(format!("http://{address}"))
}

/// One HTTP request captured by [`spawn_recording_mock_server`].
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordedRequest {
    /// The request headers, names lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The raw request body.
    pub body: String,
}

impl RecordedRequest {
    /// Returns the first value of one header, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| value.as_str())
    }

    /// Parses the body as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("recorded request body must be JSON")
    }
}

/// One canned HTTP response served by [`spawn_recording_mock_server`].
#[derive(Clone, Debug)]
pub(crate) struct MockHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl MockHttpResponse {
    /// A `200` server-sent-event stream.
    pub fn sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: body.to_string(),
        }
    }
}

/// Spawns an HTTP mock server that answers one connection per canned response,
/// in order, and records every request's headers and body.
pub(crate) async fn spawn_recording_mock_server(
    responses: Vec<MockHttpResponse>,
) -> Result<(String, Arc<Mutex<Vec<RecordedRequest>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("server should accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let head_end = loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("request read should succeed");
                if read == 0 {
                    break request.len();
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let head = String::from_utf8(request[..head_end].to_vec()).expect("head is utf-8");
            let headers = head
                .lines()
                .skip(1)
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
                })
                .collect::<Vec<_>>();
            let content_length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or_default();
            let mut body = request[head_end.min(request.len())..].to_vec();
            while body.len() < content_length {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("body read should succeed");
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&buffer[..read]);
            }
            sink.lock().push(RecordedRequest {
                headers,
                body: String::from_utf8(body).expect("body must be utf-8"),
            });
            let extra_headers = response
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect::<String>();
            let raw = format!(
                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{}",
                response.status,
                response.body.len(),
                response.body
            );
            socket
                .write_all(raw.as_bytes())
                .await
                .expect("response write should succeed");
        }
    });

    Ok((format!("http://{address}"), recorded))
}

/// Spawns a one-shot chunked HTTP mock server that captures the request body.
pub(crate) async fn spawn_chunked_mock_server(
    headers: &[(&str, &str)],
    body_chunks: Vec<Vec<u8>>,
    captured_request_body: Arc<Mutex<String>>,
) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let headers = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("server should accept");
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("request read should succeed");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request_text = String::from_utf8(request).expect("request must be utf-8");
        let content_length = request_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then_some(value)
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or_default();
        let body_start = request_text
            .find("\r\n\r\n")
            .map(|index| index + 4)
            .unwrap_or(request_text.len());
        let mut body_bytes = request_text.as_bytes()[body_start..].to_vec();
        while body_bytes.len() < content_length {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("body read should succeed");
            if read == 0 {
                break;
            }
            body_bytes.extend_from_slice(&buffer[..read]);
        }
        *captured_request_body.lock() = String::from_utf8(body_bytes).expect("body must be utf-8");

        let response_head =
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n{headers}\r\n");
        socket
            .write_all(response_head.as_bytes())
            .await
            .expect("response head write should succeed");
        for chunk in body_chunks {
            let chunk_size = format!("{:X}\r\n", chunk.len());
            socket
                .write_all(chunk_size.as_bytes())
                .await
                .expect("chunk size write should succeed");
            socket
                .write_all(&chunk)
                .await
                .expect("chunk body write should succeed");
            socket
                .write_all(b"\r\n")
                .await
                .expect("chunk delimiter write should succeed");
        }
        socket
            .write_all(b"0\r\n\r\n")
            .await
            .expect("final chunk write should succeed");
    });

    Ok(format!("http://{address}"))
}
