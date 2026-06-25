use serde_json::Value;

use crate::model::ProviderError;

/// One JSON server-sent event.
#[derive(Clone, Debug)]
pub(crate) struct JsonSseEvent {
    /// The event type label.
    pub event_type: String,
    /// The parsed JSON payload.
    pub payload: Value,
}

/// Removes the next complete SSE frame from the in-memory buffer.
pub(crate) fn pop_sse_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let (frame_end, delimiter_len) = find_frame_boundary(buffer)?;
    let frame = buffer[..frame_end].to_vec();
    buffer.drain(..frame_end + delimiter_len);
    Some(frame)
}

/// Parses one SSE frame whose data payload is JSON.
pub(crate) fn parse_json_sse_frame(
    frame: &[u8],
    provider_name: &str,
) -> Result<Option<JsonSseEvent>, ProviderError> {
    let frame = std::str::from_utf8(frame).map_err(|error| ProviderError {
        message: format!("invalid UTF-8 in {provider_name} SSE frame: {error}"),
        retryable: true,
        retry_after_ms: None,
    })?;
    let mut event_type = None;
    let mut data_lines = Vec::new();
    for line in frame.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        }
    }

    if data_lines.is_empty() {
        return Ok(None);
    }

    let payload_text = data_lines.join("\n");
    if payload_text == "[DONE]" {
        return Ok(None);
    }

    let payload: Value = serde_json::from_str(&payload_text).map_err(|error| ProviderError {
        message: format!("failed to parse {provider_name} SSE payload: {error}"),
        retryable: true,
        retry_after_ms: None,
    })?;
    let event_type = event_type
        .or_else(|| {
            payload
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    Ok(Some(JsonSseEvent {
        event_type,
        payload,
    }))
}

fn find_frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let crlf = find_subsequence(buffer, b"\r\n\r\n").map(|index| (index, 4));
    let lf = find_subsequence(buffer, b"\n\n").map(|index| (index, 2));
    match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(if crlf.0 <= lf.0 { crlf } else { lf }),
        (Some(crlf), None) => Some(crlf),
        (None, Some(lf)) => Some(lf),
        (None, None) => None,
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{find_frame_boundary, parse_json_sse_frame, pop_sse_frame};

    #[test]
    fn pop_sse_frame_accepts_utf8_split_across_buffer_appends() {
        let frame = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"café\"}\n\n"
        );
        let split_at = frame.find("é").expect("fixture should contain é") + 1;
        let mut buffer = frame.as_bytes()[..split_at].to_vec();
        assert!(find_frame_boundary(&buffer).is_none());

        buffer.extend_from_slice(&frame.as_bytes()[split_at..]);
        let popped = pop_sse_frame(&mut buffer).expect("frame should be available");
        assert!(buffer.is_empty());

        let event = parse_json_sse_frame(&popped, "OpenAI")
            .expect("frame should decode")
            .expect("frame should contain JSON data");
        assert_eq!(event.event_type, "response.output_text.delta");
        assert_eq!(event.payload["delta"], "café");
    }
}
