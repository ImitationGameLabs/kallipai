//! Two-layer auth: Authentication (who are you) then Authorization (can you do this).

use axum::extract::FromRequestParts;
use axum::http::StatusCode;

use crate::state::{AgentId, SharedState};

/// Resolved identity from the Authorization header.
#[derive(Debug, Clone)]
pub enum Identity {
    /// Caller authenticated as the operator (superuser).
    Operator,
    /// Caller authenticated as a specific agent.
    Agent { id: AgentId },
}

/// axum extractor that resolves a Bearer token to an [`Identity`].
///
/// Layer 1 (Authentication): parses the `Authorization: Bearer <token>` header,
/// matches against `operator_token` first, then checks the registry token index.
#[derive(Debug, Clone)]
pub struct AuthIdentity(pub Identity);

impl FromRequestParts<SharedState> for AuthIdentity {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_token(&parts.headers)?;

        // NOTE: Non-constant-time comparison is acceptable: agents authenticate
        // over localhost, and operator access over open networks will require
        // HTTPS. In neither case is timing a practical attack vector.
        if state.operator_token == token {
            return Ok(AuthIdentity(Identity::Operator));
        }

        let registry = state.registry.read().await;
        if let Some(id) = registry.get_agent_id_by_token(token) {
            return Ok(AuthIdentity(Identity::Agent { id: id.clone() }));
        }

        Err((StatusCode::UNAUTHORIZED, "invalid agent token".into()))
    }
}

// ---------------------------------------------------------------------------
// Layer 2: Authorization helpers
// ---------------------------------------------------------------------------

/// Only the operator may proceed. Used for root agent creation.
pub fn require_operator(identity: &Identity) -> Result<(), (StatusCode, String)> {
    match identity {
        Identity::Operator => Ok(()),
        Identity::Agent { .. } => Err((
            StatusCode::FORBIDDEN,
            "only operators can create root agents".into(),
        )),
    }
}

/// Extract bearer token from the Authorization header.
fn extract_token(headers: &axum::http::HeaderMap) -> Result<&str, (StatusCode, String)> {
    let value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::UNAUTHORIZED, "authentication required".into()))?;
    let token = value.strip_prefix("Bearer ").ok_or((
        StatusCode::UNAUTHORIZED,
        "invalid Authorization scheme, expected Bearer".into(),
    ))?;
    if token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "empty bearer token".into()));
    }
    Ok(token)
}
