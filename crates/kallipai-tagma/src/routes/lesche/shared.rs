//! What every lesche face shares: the two guards, the negative-position
//! refusal, and the error mapping.
//!
//! The guards split on the write/read line: the agent's user-facing and room
//! voice is the root's alone ([`require_root_self`]), while the console UI
//! reaches the read surfaces through the manage bridge as the operator
//! ([`require_root_self_or_operator`]).

use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ApiError;

use crate::state::SharedState;

/// Self-only AND root-only guard shared by the lesche routes (the agent's
/// user-facing / room voice is owned by the single root agent; subagents route
/// outward communication through their supervisor, and the operator is not
/// allowed to forge the agent's voice).
pub(super) async fn require_root_self(
    state: &SharedState,
    identity: &crate::auth::Identity,
    id: &AgentId,
) -> Result<(), ApiError> {
    let is_root = {
        let registry = state.registry.read().await;
        registry.require_self(identity, id)?;
        registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == id)
    };
    if !is_root {
        return Err(ApiError::forbidden(
            "delivering messages to the user requires the root agent; \
             subagents must route outward communication through their supervisor",
        ));
    }
    Ok(())
}

/// Read-side variant of [`require_root_self`] for the two lesche read
/// routes the console UI reaches through the daemon manage bridge (the
/// bridge authenticates as the operator): the operator may read the root
/// agent's lesche surfaces on the owner's behalf, but the write paths
/// stay [`require_root_self`]-guarded -- an operator posting as the
/// agent would forge the agent's voice.
pub(super) async fn require_root_self_or_operator(
    state: &SharedState,
    identity: &crate::auth::Identity,
    id: &AgentId,
) -> Result<(), ApiError> {
    let is_root = {
        let registry = state.registry.read().await;
        registry.require_self_or_operator(identity, id)?;
        registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == id)
    };
    if !is_root {
        return Err(ApiError::forbidden(
            "reading lesche surfaces requires the root agent",
        ));
    }
    Ok(())
}

/// Refuse a position the log cannot have.
///
/// A position is an ordinal in a log, so a negative one is not a direction: the
/// lesche would read it as "from the start" and answer a page nobody asked for.
/// Both reads take the answer from here -- the room read on either cursor, the
/// direct read on the one cursor it has -- so the status and the sentence
/// cannot drift apart, and the refusal is this request's mistake rather than an
/// upstream fault (the lesche's own 400 would reach the caller as a 502).
pub(super) fn refuse_negative_position(positions: &[Option<i64>]) -> Result<(), ApiError> {
    if positions.iter().flatten().any(|seq| *seq < 0) {
        return Err(ApiError::bad_request("a position cannot be negative"));
    }
    Ok(())
}

/// Map a lesche-client error to a precise [`ApiError`]: a member-gated 404
/// (unknown room/session / not a member) surfaces as `not_found` rather than
/// a misleading 502 bad-gateway; everything else stays a 502 (the lesche
/// itself is the transport in question).
pub(super) fn map_lesche_error(context: &str, e: &anyhow::Error) -> ApiError {
    if let Some(err) = e.downcast_ref::<kallipai_lesche_client::LescheHttpError>()
        && err.status == reqwest::StatusCode::NOT_FOUND
    {
        return ApiError::not_found(format!("{context}: unknown target or not a member"));
    }
    ApiError::bad_gateway(format!("{context}: {e:#}"))
}
