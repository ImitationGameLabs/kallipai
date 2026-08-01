//! Auth for the management API: a thin extractor that captures the raw bearer,
//! plus a shared `verify` helper that asks the tagma to confirm the caller's
//! claimed `(agent_id, token)` pair. Mirrors tagma's handler-level
//! authorization (AuthIdentity + id checked in the handler, not an extractor).

use axum::extract::FromRequestParts;
use kallip_common::agentid::AgentId;
use kallip_common::auth_header::extract_bearer_token;
use kallip_common::protocol::ApiError;

use crate::state::SharedState;

/// The raw bearer credential from `Authorization: Bearer <token>`. Carries no
/// identity; the handler pairs it with the claimed agent id and calls
/// [`verify`] to confirm.
#[derive(Debug, Clone)]
pub struct AuthIdentity(pub String);

impl FromRequestParts<SharedState> for AuthIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(&parts.headers)?;
        Ok(AuthIdentity(token.to_string()))
    }
}

/// Verify `(bearer, id)` against the tagma: `Ok(())` on match, `401` on
/// mismatch, `503` if the tagma is unreachable (a tagma outage is not an auth
/// failure). No caching — a verified id that survived agent removal would be a
/// hole.
pub async fn verify(state: &SharedState, bearer: &str, id: &AgentId) -> Result<(), ApiError> {
    let client = kallip_client::TagmaClient::builder(&state.tagma_url)
        .auth_token(bearer)
        .build()
        .map_err(ApiError::internal)?;
    client.verify_agent(id).await.map_err(|e| {
        if let Some(api) = e.downcast_ref::<ApiError>()
            && api.status == 401
        {
            return ApiError::unauthorized("agent id does not match token");
        }
        // Transport / timeout / unexpected — tagma-side, not an auth failure.
        ApiError::unavailable(format!("tagma verify failed: {e}"))
    })
}
