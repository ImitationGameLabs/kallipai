//! Two-layer auth: Authentication (who are you) then Authorization (can you do this).

use axum::extract::FromRequestParts;
use axum::http::StatusCode;

use crate::state::{AgentEntry, SharedState};

/// Resolved identity from the Authorization header.
#[derive(Debug, Clone)]
pub enum Identity {
    /// Caller authenticated as the operator (superuser).
    Operator,
    /// Caller authenticated as a specific agent.
    Agent { id: String },
}

/// axum extractor that resolves a Bearer token to an [`Identity`].
///
/// Layer 1 (Authentication): parses the `Authorization: Bearer <token>` header,
/// matches against `operator_token` first, then against all agent `auth_token`s.
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

        let agents = state.agents.read().await;
        if let Some(entry) = agents.iter().find(|e| e.agent.auth_token == token) {
            return Ok(AuthIdentity(Identity::Agent { id: entry.id.clone() }));
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

/// Caller must be the operator or the direct supervisor of the subagent being created.
/// Returns the supervisor's `AgentEntry` for delegation checks.
pub fn require_supervisor<'a>(
    identity: &Identity,
    agents: &'a [AgentEntry],
    supervisor_id: &str,
) -> Result<&'a AgentEntry, (StatusCode, String)> {
    let supervisor = agents.iter().find(|e| e.id == supervisor_id).ok_or((
        StatusCode::NOT_FOUND,
        format!("supervisor agent {supervisor_id} not found"),
    ))?;
    match identity {
        Identity::Operator => Ok(supervisor),
        Identity::Agent { id } if id == supervisor_id => Ok(supervisor),
        _ => Err((
            StatusCode::FORBIDDEN,
            "invalid auth token for supervisor agent".into(),
        )),
    }
}

/// Caller must be the operator or a superior of the target agent.
/// Walks the `created_by` chain upward from the target.
pub fn require_superior(
    identity: &Identity,
    agents: &[AgentEntry],
    target_id: &str,
) -> Result<(), (StatusCode, String)> {
    match identity {
        Identity::Operator => return Ok(()),
        Identity::Agent { id: caller_id } => {
            let mut current_id = target_id.to_owned();
            loop {
                let entry = match agents.iter().find(|e| e.id == current_id) {
                    Some(e) => e,
                    None => return Err((StatusCode::FORBIDDEN, "broken supervisor chain".into())),
                };
                match &entry.agent.config.created_by {
                    Some(supervisor_id) => {
                        if supervisor_id == caller_id {
                            return Ok(());
                        }
                        current_id = supervisor_id.clone();
                    }
                    None => break,
                }
            }
        }
    }

    Err((
        StatusCode::FORBIDDEN,
        "not authorized to manage this agent".into(),
    ))
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
