//! The direct-session face: one conversation with a peer tagma, read by pair
//! (`--peer`) or by the session id a delivered header already spells.
//!
//! Both entry points answer the query's own shape first and then share
//! [`read_direct_session`], which picks the relay that owns the session and asks
//! it for one page. The text render and the JSON rows it can also ask for live
//! in [`super::render`].

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::response::Response;
use kallipai_archeion_common::ids::TagmaId;
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ApiError;
use kallipai_lesche_common::direct::DirectSessionId;
use serde::Deserialize;

use crate::state::SharedState;

use super::render::{DirectMessageJsonRow, direct_json_row, render_direct_text};
use super::shared::{map_lesche_error, refuse_negative_position, require_root_self_or_operator};

/// Query for `GET /agents/{id}/lesche/direct-sessions/{peer}/messages`: the
/// room history's paging fields plus `format=json` — the console UI's
/// structured variant. No `format` (any other value → 400) keeps the text
/// render, so the CLI contract does not move.
#[derive(Debug, Deserialize)]
pub(crate) struct DirectHistoryQuery {
    /// Return messages with `seq > after_seq` (exclusive). Default 0 = from
    /// the start.
    pub after_seq: Option<i64>,
    /// Max messages to return. Server-clamped by lesche.
    pub limit: Option<u64>,
    /// `json` switches the response from the bracketed text render to typed
    /// rows ([`DirectMessageJsonRow`]).
    pub format: Option<String>,
}

/// The json variant's default page (no `limit` query): small enough that a
/// full page fits the manage bridge's 256 KiB response cap with headroom,
/// large enough to spare a request per render. The text render keeps the
/// lesche's own default (the CLI contract does not move).
pub(super) const DIRECT_JSON_DEFAULT_LIMIT: u64 = 25;

/// `GET /agents/{id}/lesche/direct-sessions/{peer}/messages` — the
/// `kallip lesche read --tagma <peer>` path. The session id is derived
/// locally from (self, peer) with the lesche-common derivation, so the
/// agent never handles a session id: the peer is the tagma id that
/// `kallip lesche sessions` pairs with the session (each direct line reads
/// `direct <session-id> peer=<tagma-id> "<handle>"`), not the participant
/// id in the inbound header's parens. Served by the relay whose poll warmed
/// direct-session cache; a cold cache is indistinguishable from "no relay
/// knows this session" and surfaces as unavailable -- self-correcting on
/// the next poll, same as the room read.
pub async fn read_direct_session_messages(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, peer_str)): Path<(AgentId, String)>,
    Query(q): Query<DirectHistoryQuery>,
) -> Result<Response, ApiError> {
    require_root_self_or_operator(&state, auth.identity(), &id).await?;
    // The query's own shape is answered first: a malformed one is this
    // request's fault whether or not a relay happens to be installed, and a
    // missing relay would blame the wrong party.
    let json = direct_read_query(&q)?;
    // TagmaId parses leniently (an opaque id string); an unknown peer
    // surfaces from the lesche history read as a 404, not a 400.
    let peer = TagmaId::from(peer_str);
    let Some(first) = state.relay_handles().first().cloned() else {
        return Err(ApiError::unavailable(
            "no relay installed; cannot read a direct session",
        ));
    };
    let session = DirectSessionId::for_pair(first.tagma_id(), &peer);
    read_direct_session(&state, &session, q, json).await
}

/// `GET /agents/{id}/lesche/sessions/{session}/messages` -- the same read
/// addressed by the session id itself (`kallip lesche read --session <session>
/// --quote <seq>`).
///
/// The pair-addressed route derives the session from (self, peer); this one is
/// handed the id an inbound header already spells (`direct <session>`), which
/// is what a delivered message's read-back command names. Nothing is derived
/// and nothing is looked up, so that command cannot land on another
/// conversation.
pub async fn read_direct_session_messages_by_id(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, session)): Path<(AgentId, String)>,
    Query(q): Query<DirectHistoryQuery>,
) -> Result<Response, ApiError> {
    require_root_self_or_operator(&state, auth.identity(), &id).await?;
    // Same answer as the pair-addressed route: the query is the caller's own
    // to get wrong, before anything is looked up.
    let json = direct_read_query(&q)?;
    // The id was spelled by whoever wrote the command, and nothing here
    // derived it: a value that is not a canonical session id cannot name a
    // session, so it is refused as this request's mistake rather than carried
    // into a member-set lookup that could only say no to it.
    let Some(session) = DirectSessionId::parse(&session) else {
        return Err(ApiError::bad_request("not a canonical session id"));
    };
    read_direct_session(&state, &session, q, json).await
}

/// Whether the query asks for the json variant, and the refusals a query can
/// earn on its own.
///
/// Only `json` is accepted: a future format must not silently mean the text
/// render, and a negative position is refused by [`refuse_negative_position`].
/// Both entry points ask this before they resolve a relay, so a malformed query
/// is answered as such rather than as a relay that happens to be missing.
fn direct_read_query(q: &DirectHistoryQuery) -> Result<bool, ApiError> {
    refuse_negative_position(&[q.after_seq])?;
    match q.format.as_deref() {
        None => Ok(false),
        Some("json") => Ok(true),
        Some(other) => Err(ApiError::bad_request(format!(
            "unsupported format: {other} (only json)"
        ))),
    }
}

/// The shared body of both direct-session reads: the relay that owns the
/// session, one page from the lesche, and the render the query asks for.
async fn read_direct_session(
    state: &SharedState,
    session: &DirectSessionId,
    q: DirectHistoryQuery,
    json: bool,
) -> Result<Response, ApiError> {
    let client = {
        let Some(owner) = state.direct_sessions.owner_of(session).await else {
            return Err(ApiError::unavailable(
                "no online relay knows this direct session; cannot read its history",
            ));
        };
        let Some(handle) = state.relay(&owner) else {
            return Err(ApiError::unavailable(
                "relay not online; cannot read direct session history",
            ));
        };
        handle.lesche_client()
    };
    // The json variant pins a smaller default page (DIRECT_JSON_DEFAULT_LIMIT):
    // the console UI pulls conservatively because the manage bridge caps
    // responses at 256 KiB and drops an oversized page whole. An explicit
    // limit always wins.
    let limit = q
        .limit
        .or_else(|| json.then_some(DIRECT_JSON_DEFAULT_LIMIT));
    let rows = client
        .fetch_direct_messages(session, q.after_seq, limit)
        .await
        .map_err(|e| map_lesche_error("direct session history fetch failed", &e))?;
    if json {
        // Typed rows for the console UI; unparseable rows are skipped, never
        // fatal (same rule as the text render).
        let view: Vec<DirectMessageJsonRow> = rows
            .into_iter()
            .filter_map(|row| direct_json_row(session, row))
            .collect();
        Ok(Json(view).into_response())
    } else {
        Ok(render_direct_text(session, rows).into_response())
    }
}
