//! Stable CLI exit-code mapping.

use std::process::ExitCode;

use anyhow::Error;

use super::http::DaemonHttpError;

const EXIT_GENERIC: u8 = 1;
const EXIT_DAEMON_UNAVAILABLE: u8 = 3;
const EXIT_TIMEOUT: u8 = 7;
const EXIT_SCHEMA_MISMATCH: u8 = 8;

pub(crate) fn exit_code_for_error(error: &Error) -> ExitCode {
    for cause in error.chain() {
        if cause
            .downcast_ref::<crate::cli::commands::runtime::DaemonStatusDecodeError>()
            .is_some()
        {
            return ExitCode::from(EXIT_SCHEMA_MISMATCH);
        }
        if let Some(http) = cause.downcast_ref::<DaemonHttpError>() {
            return ExitCode::from(http.stable_exit_code());
        }
        if let Some(reqwest) = cause.downcast_ref::<reqwest::Error>() {
            if reqwest.is_timeout() {
                return ExitCode::from(EXIT_TIMEOUT);
            }
            if reqwest.is_connect() || reqwest.is_request() {
                return ExitCode::from(EXIT_DAEMON_UNAVAILABLE);
            }
        }
    }
    let message = error.to_string();
    if message.contains("failed to decode daemon response")
        || message.contains("failed to decode daemon status response")
    {
        return ExitCode::from(EXIT_SCHEMA_MISMATCH);
    }
    ExitCode::from(EXIT_GENERIC)
}

#[cfg(test)]
mod tests {
    use anyhow::Error;

    use super::*;

    #[test]
    fn http_problem_errors_map_to_stable_exit_codes() {
        let not_found: Error = DaemonHttpError::from_body(
            reqwest::StatusCode::NOT_FOUND,
            "unknown session demo".to_string(),
        )
        .into();
        assert_eq!(exit_code_for_error(&not_found), ExitCode::from(5));

        let conflict: Error = DaemonHttpError::from_body(
            reqwest::StatusCode::CONFLICT,
            "run already active".to_string(),
        )
        .into();
        assert_eq!(exit_code_for_error(&conflict), ExitCode::from(6));

        let rate_limited: Error = DaemonHttpError::from_body(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "rate limited".to_string(),
        )
        .into();
        assert_eq!(exit_code_for_error(&rate_limited), ExitCode::from(7));

        let unavailable: Error = DaemonHttpError::from_body(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "daemon is draining".to_string(),
        )
        .into();
        assert_eq!(exit_code_for_error(&unavailable), ExitCode::from(7));
    }

    #[test]
    fn wrapped_http_problem_errors_keep_stable_exit_codes() {
        let wrapped: Error = anyhow::anyhow!(DaemonHttpError::from_body(
            reqwest::StatusCode::UNAUTHORIZED,
            "missing daemon token".to_string(),
        ))
        .context("doctor could not fetch daemon status");
        assert_eq!(exit_code_for_error(&wrapped), ExitCode::from(4));
    }

    #[test]
    fn status_schema_decode_errors_map_to_stable_exit_code() {
        let error: Error = anyhow::anyhow!("missing field `status`")
            .context("failed to decode daemon status response");
        assert_eq!(exit_code_for_error(&error), ExitCode::from(8));
    }

    #[test]
    fn typed_status_schema_decode_errors_map_to_stable_exit_code() {
        let error: Error = crate::cli::commands::runtime::DaemonStatusDecodeError::new(
            "failed to decode daemon status response",
        )
        .into();
        assert_eq!(exit_code_for_error(&error), ExitCode::from(8));
    }
}
