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
