//! `GET /team/inactive-agents`: the inactive half of the team topology.
//!
//! Read face, authenticate-only — the same baseline as `team_status` and
//! `list_agents` (the live half of the same topology; same caveat as the
//! agent list: the listing carries no secrets, id + role only, and the
//! exposure must be revisited if agent listing is ever scoped
//! per-caller). The write face stays operator/root.
//!
//! This listing is not serialized against converge: a converge may be in
//! flight while it is read. Per-entry visibility flips atomically with
//! the body's own park/unpark, never mid-snapshot.

use axum::Json;
use axum::extract::State;
use kallipai_common::protocol::{ApiError, TeamInactiveAgent, TeamInactiveAgentsListing};

use crate::state::SharedState;

/// List every body currently in the inactive area, machine-wide. A scan
/// failure fails loud (500): lock rebuild is the substrate consumer, and
/// rebuilding without this set would silently absorb inactive agents.
pub(super) async fn list_inactive_agents(
    State(_state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> Result<Json<TeamInactiveAgentsListing>, ApiError> {
    let entries = kallipai_adk::persistence::scan_inactive()
        .map_err(|e| ApiError::internal(format!("inactive-area scan failed: {e:#}")))?;
    Ok(Json(TeamInactiveAgentsListing {
        inactive_agents: entries
            .into_iter()
            .map(|(id, meta)| TeamInactiveAgent {
                id,
                role: meta.role,
            })
            .collect(),
    }))
}
