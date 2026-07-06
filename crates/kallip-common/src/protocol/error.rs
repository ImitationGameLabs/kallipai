//! Structured API error type for daemon-client communication.
//!
//! All daemon HTTP routes return errors as [`ApiError`], which serializes to
//! `{"error":{"message":"..."}}` via the `IntoResponse` impl (gated behind the
//! `axum` feature). The client library deserializes this envelope to produce
//! typed errors instead of opaque status-code checks.
//!
//! A machine-readable `code` field is intentionally deferred — the HTTP status
//! code alone provides sufficient classification for the current API scale
//! (~14 endpoints). It can be added non-breaking later.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Structured error returned by the daemon HTTP API.
///
/// On the wire the body is `{"error":{"message":"..."}}` with the status code
/// carried by the HTTP response line (not duplicated in the JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    /// HTTP status code (not serialized — carried by the response line).
    #[serde(skip)]
    pub status: u16,
    /// Human-readable error description.
    pub message: String,
}

impl ApiError {
    // -- Constructors by status ------------------------------------------------

    /// 400 Bad Request
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: msg.into(),
        }
    }

    /// 401 Unauthorized
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: 401,
            message: msg.into(),
        }
    }

    /// 403 Forbidden
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: 403,
            message: msg.into(),
        }
    }

    /// 404 Not Found
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: 404,
            message: msg.into(),
        }
    }

    /// 409 Conflict
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: 409,
            message: msg.into(),
        }
    }

    /// 500 Internal Server Error.
    ///
    /// Logs the full error detail via `tracing::error!` and returns a generic
    /// `"internal error"` message to the client. This prevents leaking
    /// internal implementation details.
    pub fn internal(e: impl fmt::Display) -> Self {
        tracing::error!("internal error: {e}");
        Self {
            status: 500,
            message: "internal error".into(),
        }
    }

    /// 503 Service Unavailable
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: 503,
            message: msg.into(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "daemon returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

// ---------------------------------------------------------------------------
// Axum integration (optional)
// ---------------------------------------------------------------------------

#[cfg(feature = "axum")]
mod axum_impl {
    use super::ApiError;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};

    /// JSON envelope matching the wire format: `{"error":{"message":"..."}}`.
    #[derive(serde::Serialize)]
    struct ErrorEnvelope {
        error: ApiError,
    }

    impl IntoResponse for ApiError {
        fn into_response(self) -> Response {
            let status =
                StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, axum::Json(ErrorEnvelope { error: self })).into_response()
        }
    }
}
