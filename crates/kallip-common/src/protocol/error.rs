//! Structured API error type for tagma-client communication.
//!
//! All tagma HTTP routes return errors as [`ApiError`], which serializes to
//! `{"error":{"message":"..."}}` via the `IntoResponse` impl (gated behind the
//! `axum` feature). The client library deserializes this envelope to produce
//! typed errors instead of opaque status-code checks.
//!
//! An optional machine-readable `code` exists for rejections that must be
//! distinguishable beyond the status line (the gateway's explicit
//! key-lifecycle rejections); it is absent on every other error.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Structured error returned by the tagma HTTP API.
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
    /// Stranded profile-set bindings, carried only by the 409 that
    /// `PUT /profiles` returns when a wholesale save would strand agents
    /// (absent on every other error). Serialized inside the error envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangling: Option<Vec<String>>,
    /// Machine-readable rejection code (`key_expired`, `key_revoked` on
    /// the gateway): the deferred `code` slot, added non-breaking for
    /// the explicit key-rejection contract; absent on every other error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ApiError {
    // -- Constructors by status ------------------------------------------------

    /// 400 Bad Request
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 401 Unauthorized
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: 401,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 401 Unauthorized carrying a machine-readable rejection code: the
    /// gateway's explicit key-rejection contract (`key_revoked`,
    /// `key_expired`) -- the client recovery path is refetching its
    /// configuration (a fresh key), not retrying the same bearer.
    pub fn unauthorized_with_code(msg: impl Into<String>, code: &str) -> Self {
        Self {
            status: 401,
            message: msg.into(),
            dangling: None,
            code: Some(code.to_owned()),
        }
    }

    /// 403 Forbidden
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: 403,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 404 Not Found
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: 404,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 409 Conflict
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: 409,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 409 Conflict carrying a machine-readable rejection code: the UI
    /// renders a localized short form instead of the operator-facing text
    /// (the CLI keeps the full message).
    pub fn conflict_with_code(msg: impl Into<String>, code: &str) -> Self {
        Self {
            status: 409,
            message: msg.into(),
            dangling: None,
            code: Some(code.to_owned()),
        }
    }

    /// 409 Conflict carrying the stranded profile-set bindings from a
    /// non-forced `PUT /profiles` (see the tagma profiles routes).
    pub fn conflict_dangling(msg: impl Into<String>, dangling: Vec<String>) -> Self {
        Self {
            status: 409,
            message: msg.into(),
            dangling: Some(dangling),
            code: None,
        }
    }

    /// 429 Too Many Requests
    pub fn too_many_requests(msg: impl Into<String>) -> Self {
        Self {
            status: 429,
            message: msg.into(),
            dangling: None,
            code: None,
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
            dangling: None,
            code: None,
        }
    }

    /// 503 Service Unavailable
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: 503,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 502 Bad Gateway (an upstream relay delivery failed).
    pub fn bad_gateway(msg: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }

    /// 504 Gateway Timeout
    pub fn gateway_timeout(msg: impl Into<String>) -> Self {
        Self {
            status: 504,
            message: msg.into(),
            dangling: None,
            code: None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tagma returned {}: {}", self.status, self.message)
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
