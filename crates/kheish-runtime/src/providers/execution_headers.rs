//! Execution-context headers carried by every model provider request.
//!
//! A gateway in front of a provider (for example an embedder's model relay)
//! needs to know which session and which daemon run a model request belongs
//! to, so it can authorize and account for it. Kheish asserts both on every
//! request from its own execution scope; the model never sees these values and
//! cannot produce them.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::execution::current_execution_scope;
use crate::model::ProviderError;

/// Header naming the session whose run issued the model request.
pub const KHEISH_SESSION_ID_HEADER: &str = "x-kheish-session-id";
/// Header naming the daemon run that issued the model request.
pub const KHEISH_RUN_ID_HEADER: &str = "x-kheish-run-id";

/// Sets the execution-context headers on an outgoing provider request.
///
/// The session comes from the current execution scope, or from the request's
/// own conversation outside a scope; the run comes from the scope only, and is
/// omitted when the request does not belong to a daemon run (a CLI turn, for
/// example). The values replace any header of the same name already present,
/// so auth material cannot shadow them.
pub(crate) fn insert_execution_context_headers(
    headers: &mut HeaderMap,
    request_session_id: &str,
) -> Result<(), ProviderError> {
    let scope = current_execution_scope();
    let session_id = scope
        .as_ref()
        .map(|scope| scope.session_id.as_str())
        .filter(|session_id| !session_id.is_empty())
        .unwrap_or(request_session_id);
    let run_id = scope
        .as_ref()
        .and_then(|scope| scope.run_id.as_deref())
        .filter(|run_id| !run_id.is_empty());
    insert(headers, KHEISH_SESSION_ID_HEADER, Some(session_id))?;
    insert(headers, KHEISH_RUN_ID_HEADER, run_id)
}

fn insert(
    headers: &mut HeaderMap,
    name: &'static str,
    value: Option<&str>,
) -> Result<(), ProviderError> {
    let name = HeaderName::from_static(name);
    headers.remove(&name);
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let value = HeaderValue::from_str(value).map_err(|error| ProviderError {
        message: format!("invalid execution context header `{name}`: {error}"),
        retryable: false,
        retry_after_ms: None,
    })?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue};
    use tokio_util::sync::CancellationToken;

    use super::{KHEISH_RUN_ID_HEADER, KHEISH_SESSION_ID_HEADER, insert_execution_context_headers};
    use crate::execution::{ExecutionScope, scope_execution};

    #[tokio::test]
    async fn scope_supplies_the_session_and_run_and_overrides_material_headers() {
        let scope = ExecutionScope {
            session_id: "fm-mission-1".to_string(),
            run_id: Some("run-7".to_string()),
            ..ExecutionScope::default()
        };
        let headers = scope_execution(scope, CancellationToken::new(), async {
            let mut headers = HeaderMap::new();
            headers.insert(KHEISH_RUN_ID_HEADER, HeaderValue::from_static("spoofed"));
            insert_execution_context_headers(&mut headers, "request-session")
                .expect("headers should be valid");
            headers
        })
        .await;
        assert_eq!(headers[KHEISH_SESSION_ID_HEADER], "fm-mission-1");
        assert_eq!(headers[KHEISH_RUN_ID_HEADER], "run-7");
        assert_eq!(headers.get_all(KHEISH_RUN_ID_HEADER).iter().count(), 1);
    }

    #[test]
    fn outside_a_scope_the_request_session_is_sent_and_no_run() {
        let mut headers = HeaderMap::new();
        headers.insert(KHEISH_RUN_ID_HEADER, HeaderValue::from_static("stale"));
        insert_execution_context_headers(&mut headers, "cli-session")
            .expect("headers should be valid");
        assert_eq!(headers[KHEISH_SESSION_ID_HEADER], "cli-session");
        assert!(headers.get(KHEISH_RUN_ID_HEADER).is_none());
    }

    #[test]
    fn a_value_that_is_not_a_header_value_fails_the_request() {
        let mut headers = HeaderMap::new();
        let error = insert_execution_context_headers(&mut headers, "bad\nsession")
            .expect_err("a newline is not a valid header value");
        assert!(!error.retryable);
        assert!(error.message.contains(KHEISH_SESSION_ID_HEADER));
    }
}
