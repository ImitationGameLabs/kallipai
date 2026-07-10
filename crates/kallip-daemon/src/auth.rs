//! Two-layer auth: Authentication (who are you) then Authorization (can you do this).

use axum::extract::FromRequestParts;

use crate::state::{AgentId, SharedState};
use crate::token::TokenHash;
use kallip_common::protocol::ApiError;

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
/// hashes it, and matches against the operator token hash first, then the
/// registry's token-hash index.
#[derive(Debug, Clone)]
pub struct AuthIdentity(Identity);

impl AuthIdentity {
    /// Access the resolved [`Identity`].
    pub fn identity(&self) -> &Identity {
        &self.0
    }

    /// Construct an [`AuthIdentity`] for testing.
    #[cfg(test)]
    pub(crate) fn test_new(identity: Identity) -> Self {
        Self(identity)
    }
}

impl FromRequestParts<SharedState> for AuthIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_token(&parts.headers)?;

        // Compare SHA-256 hashes: the operator secret via constant-time compare
        // (subtle), agents via a HashMap lookup keyed by hash. Because an attacker
        // cannot steer a SHA-256 output, variable lookup/compare time over hashes
        // leaks nothing about the secret — timing is not a practical vector even
        // off-localhost (e.g. a 0.0.0.0 bind), superseding the old localhost/HTTPS
        // tradeoff this code once documented.
        let hash = TokenHash::of(token);
        if state.operator_token_hash.ct_eq(&hash) {
            return Ok(AuthIdentity(Identity::Operator));
        }

        let registry = state.registry.read().await;
        if let Some(id) = registry.get_agent_id_by_token(&hash) {
            return Ok(AuthIdentity(Identity::Agent { id: id.clone() }));
        }

        Err(ApiError::unauthorized("invalid agent token"))
    }
}

// ---------------------------------------------------------------------------
// Layer 2: Authorization helpers
// ---------------------------------------------------------------------------

/// Only the operator may proceed. Used for root agent creation and
/// daemon-wide resource management (e.g. token budget).
pub fn require_operator(identity: &Identity) -> Result<(), ApiError> {
    match identity {
        Identity::Operator => Ok(()),
        Identity::Agent { .. } => Err(ApiError::forbidden("operator access required")),
    }
}

/// Extract bearer token from the Authorization header.
fn extract_token(headers: &axum::http::HeaderMap) -> Result<&str, ApiError> {
    let value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("authentication required"))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("invalid Authorization scheme, expected Bearer"))?;
    if token.is_empty() {
        return Err(ApiError::unauthorized("empty bearer token"));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{make_entry, make_state};
    use axum::http::Request;

    /// Build request parts carrying `Authorization: Bearer <token>`.
    fn parts_with_bearer(token: &str) -> axum::http::request::Parts {
        Request::builder()
            .header("authorization", format!("Bearer {token}"))
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[tokio::test]
    async fn operator_token_resolves_to_operator_identity() {
        // make_state hashes plaintext "op-token" as the operator secret.
        let state = make_state();
        let mut parts = parts_with_bearer("op-token");
        let auth = AuthIdentity::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert!(matches!(auth.identity(), &Identity::Operator));
    }

    #[tokio::test]
    async fn agent_token_resolves_to_agent_identity() {
        let state = make_state();
        let id = AgentId::random();
        let plain = format!("agent-{id}");
        {
            let mut reg = state.registry.write().await;
            reg.register(
                id.clone(),
                crate::state::RegistryEntry::Live(make_entry(None, plain.clone())),
            );
        }
        let mut parts = parts_with_bearer(&plain);
        let auth = AuthIdentity::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        match auth.identity() {
            Identity::Agent { id: resolved } => assert_eq!(resolved, &id),
            other => panic!("expected Agent identity, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_token_is_rejected() {
        let state = make_state();
        let mut parts = parts_with_bearer("definitely-not-a-real-token");
        let err = AuthIdentity::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, 401);
    }

    #[tokio::test]
    async fn missing_authorization_header_is_rejected() {
        let state = make_state();
        let mut parts = Request::builder().body(()).unwrap().into_parts().0;
        let err = AuthIdentity::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, 401);
    }
}
