//! The uniform HTTP error surface: every failure is `{code, message}` JSON
//! with the status carried by the response line.
//!
//! `code` is the machine key clients branch on: daemon `ErrorCode` names in
//! snake_case for daemon-side failures, plus proxy-level keys
//! (`daemon_unreachable`, `daemon_error`) and guard keys (`unauthorized`,
//! `host_forbidden`, `bad_request`).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kallipai_daemon_client::ClientError;
use kallipai_daemon_common::wire::ErrorCode;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ApiFault {
    pub code: &'static str,
    pub message: String,
}

/// Render one fault. Shared by handlers and both request guards.
pub fn fault(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiFault {
            code,
            message: message.into(),
        }),
    )
        .into_response()
}

/// Daemon error codes to HTTP statuses. Exhaustive on purpose: a new
/// `ErrorCode` variant breaks this match instead of silently defaulting.
pub fn daemon_code_status(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::SlugTaken | ErrorCode::NotRunning => StatusCode::CONFLICT,
        ErrorCode::Denied => StatusCode::FORBIDDEN,
        ErrorCode::WorkspaceOverlap | ErrorCode::InvalidSpawnInput => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ErrorCode::SpawnTimeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        // The daemon spoke a code this build never wrote (version skew).
        ErrorCode::Unknown => StatusCode::BAD_GATEWAY,
    }
}

/// The wire name of a code (`slug_taken`, ...). Kept in lockstep with the
/// serde `snake_case` rename on `ErrorCode`; the test below cross-checks.
pub fn daemon_code_key(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::SlugTaken => "slug_taken",
        ErrorCode::Denied => "denied",
        ErrorCode::WorkspaceOverlap => "workspace_overlap",
        ErrorCode::InvalidSpawnInput => "invalid_spawn_input",
        ErrorCode::SpawnTimeout => "spawn_timeout",
        ErrorCode::NotFound => "not_found",
        ErrorCode::NotRunning => "not_running",
        ErrorCode::BadRequest => "bad_request",
        ErrorCode::Internal => "internal",
        ErrorCode::Unknown => "unknown",
    }
}

/// Render a daemon-side error response.
pub fn daemon_err(code: ErrorCode, message: String) -> Response {
    fault(daemon_code_status(code), daemon_code_key(code), message)
}

/// Render a proxy-level failure: the daemon socket is unreachable (dead
/// daemon, wrong path) vs the exchange failed some other way (malformed or
/// oversized response, connection dropped mid-exchange).
pub fn proxy_err(error: ClientError) -> Response {
    match &error {
        ClientError::Connect { .. } => fault(
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon_unreachable",
            format!("cannot reach the kallipai daemon: {error}"),
        ),
        _ => fault(
            StatusCode::BAD_GATEWAY,
            "daemon_error",
            format!("daemon exchange failed: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_maps_to_the_planned_status() {
        let cases = [
            (ErrorCode::SlugTaken, StatusCode::CONFLICT),
            (ErrorCode::Denied, StatusCode::FORBIDDEN),
            (ErrorCode::NotRunning, StatusCode::CONFLICT),
            (
                ErrorCode::WorkspaceOverlap,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ErrorCode::InvalidSpawnInput,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (ErrorCode::SpawnTimeout, StatusCode::GATEWAY_TIMEOUT),
            (ErrorCode::NotFound, StatusCode::NOT_FOUND),
            (ErrorCode::BadRequest, StatusCode::BAD_REQUEST),
            (ErrorCode::Internal, StatusCode::INTERNAL_SERVER_ERROR),
            (ErrorCode::Unknown, StatusCode::BAD_GATEWAY),
        ];
        for (code, status) in cases {
            assert_eq!(daemon_code_status(code), status, "{code:?}");
        }
    }

    #[test]
    fn code_keys_match_wire_serialization() {
        // The HTTP `code` must equal the serde name the UDS wire uses, so a
        // TS client and the Rust wire agree without a translation table.
        for code in [
            ErrorCode::SlugTaken,
            ErrorCode::Denied,
            ErrorCode::WorkspaceOverlap,
            ErrorCode::InvalidSpawnInput,
            ErrorCode::SpawnTimeout,
            ErrorCode::NotFound,
            ErrorCode::NotRunning,
            ErrorCode::BadRequest,
            ErrorCode::Internal,
            ErrorCode::Unknown,
        ] {
            let wire = serde_json::to_value(code).expect("serialize code");
            assert_eq!(wire.as_str().expect("string code"), daemon_code_key(code));
        }
    }

    #[test]
    fn connect_failures_are_unreachable_others_are_bad_gateway() {
        let connect = ClientError::Connect {
            path: "/nonexistent.sock".into(),
            source: std::io::Error::other("no such file"),
        };
        assert_eq!(proxy_err(connect).status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            proxy_err(ClientError::Closed).status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
