//! Direct-session routes: tagma-callable create-or-get, listing, send,
//! history, and read-cursor for the two-member plaintext 1v1 spaces.
//!
//! Authorization shape: every route requires a `Principal::Tagma` (bearer);
//! unknown session / non-member / a non-tagma principal all collapse into one
//! uniform 404 -- the existence-oracle discipline the room surface uses, so a
//! probe never learns whether a session it is not party to exists. The create
//! route additionally enforces the same-owner trust domain by resolving both
//! tagmas' `owner_username` from the registry: unknown / cross-owner /
//! not-tunnel-usable peers are indistinguishable 404s.
//!
//! The send route mirrors `post_room_envelope` construction-for-construction
//! (path-authoritative channel, sender-match, authoritative handle stamp,
//! opaque plaintext payload, fan-under-read-lock, sync 202); only the
//! membership source differs (the session row's two columns instead of the
//! room graph).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kallip_archeion_common::bytes::Ciphertext;
use kallip_archeion_common::ids::{ParticipantId, ParticipantKind, TagmaId};
use kallip_archeion_common::principal::Principal;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::direct::{
    DirectMessageView, DirectSessionId, DirectSessionPeer, DirectSessionView,
};
use kallip_lesche_common::message::Envelope;
use kallip_lesche_common::rooms::{MemberId, RoomMember, RoomMembership};
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth::AuthPrincipal;
use crate::db::direct_store::DirectSessionRow;
use crate::db::{Db, direct_store, map_db_err};
use crate::fan::fan_envelope;
use crate::identity::{agent_handle, degraded_handle};
use crate::member_identity::{MemberRef, resolve_handles};
use crate::state::SharedConvState;

pub fn router() -> axum::Router<SharedConvState> {
    axum::Router::new()
        .route("/direct-sessions", axum::routing::post(post_direct_session))
        .route("/direct-sessions", axum::routing::get(list_direct_sessions))
        .route(
            "/direct-sessions/{session_id}/messages",
            axum::routing::post(post_direct_session_envelope),
        )
        .route(
            "/direct-sessions/{session_id}/messages",
            axum::routing::get(get_direct_session_messages),
        )
        .route(
            "/direct-sessions/{session_id}/read-cursor",
            axum::routing::put(put_direct_read_cursor),
        )
}

/// Body of `POST /direct-sessions`. The peer is addressed by its tagma id
/// (the CLI's `send --tagma <id>`); the caller is the initiator.
#[derive(Debug, Deserialize)]
pub struct CreateDirectSessionRequest {
    pub peer: String,
}

/// Body of `PUT /direct-sessions/{id}/read-cursor`.
#[derive(Debug, Deserialize)]
pub struct ReadCursorRequest {
    pub last_read_seq: i64,
}

/// Server cap on a single history page (the room history route's clamp).
const HISTORY_MAX_LIMIT: u64 = 200;

/// The caller must be a tagma bearer. Every other principal collapses to the
/// same 404 as an unknown session (an Admin, like a non-member, learns
/// nothing).
fn as_tagma(principal: &Principal) -> Option<&TagmaId> {
    match principal {
        Principal::Tagma(t) => Some(t),
        _ => None,
    }
}

/// Fetch the session row and gate membership in one step: unknown session and
/// non-member are the same 404 that confirms nothing.
async fn require_member_session(
    db: &Db,
    session_id: &str,
    me: &TagmaId,
) -> Result<DirectSessionRow, ApiError> {
    let row = direct_store::find_session(db, session_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown session"))?;
    if !row.has_member(me.as_ref()) {
        return Err(ApiError::not_found("unknown session"));
    }
    Ok(row)
}

/// Stamp the authoritative sender identity before persist + fan-out (the
/// rooms send path's stamp, verbatim vocabulary): the stable
/// `<id-prefix>@<owner-username>` handle from the profile cache or the
/// registry, degrading to the unforgeable id-prefix only (a registry outage
/// must not fail the send), plus the deep-link `tagma_id`.
async fn stamp_sender(
    state: &SharedConvState,
    sender: &mut kallip_lesche_common::message::Participant,
    tid: &TagmaId,
    pid: &ParticipantId,
) {
    sender.handle = match state.agent_profiles.get(pid) {
        Some(p) => agent_handle(pid, &p.owner_username),
        None => match crate::control_policy::tagma_profile(&*state.control, tid).await {
            Ok(Some(p)) if crate::control_policy::tunnel_usable(&p) => {
                let profile = crate::state::AgentProfile {
                    label: p.label,
                    owner_username: p.owner_username,
                    owner_display_name: p.owner_display_name,
                };
                let h = agent_handle(pid, &profile.owner_username);
                state.agent_profiles.set(pid.clone(), profile);
                h
            }
            _ => degraded_handle(pid, ParticipantKind::Agent),
        },
    };
    sender.tagma_id = Some(tid.clone());
}

/// Create-or-get the direct session between the caller and `req.peer`.
/// Same-owner + tunnel-usable + existence all resolve through the registry
/// and collapse into one 404 (a probe cannot distinguish "no such tagma"
/// from "not yours to reach"). The id is derived from the canonical pair, so
/// a re-create (either side initiating) lands on the same row.
async fn post_direct_session(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateDirectSessionRequest>,
) -> Result<Json<DirectSessionView>, ApiError> {
    let Some(me) = as_tagma(&principal) else {
        return Err(ApiError::not_found("unknown session"));
    };
    if req.peer == me.as_ref() {
        return Err(ApiError::bad_request(
            "cannot open a direct session with yourself",
        ));
    }
    let peer = TagmaId::from(req.peer);
    let db = state.require_db()?;

    // Both profiles resolve through the registry (the enrollment authority).
    // A registry RPC failure is a 500 -- an unverified create must not land.
    let my_profile = crate::control_policy::tagma_profile(&*state.control, me)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .ok_or_else(|| ApiError::not_found("unknown session"))?;
    let peer_profile = crate::control_policy::tagma_profile(&*state.control, &peer)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .ok_or_else(|| ApiError::not_found("unknown session"))?;
    // The v1 trust domain: same owner, both usable (enrolled, non-revoked,
    // pinned key). Anything else is the same 404 -- the caller cannot tell a
    // cross-owner peer from a nonexistent one.
    if !crate::control_policy::tunnel_usable(&my_profile)
        || !crate::control_policy::tunnel_usable(&peer_profile)
        || my_profile.owner_username != peer_profile.owner_username
    {
        return Err(ApiError::not_found("unknown session"));
    }

    let (a, b) = {
        let (x, y) = kallip_lesche_common::direct::canonical_pair(me, &peer);
        (x.as_ref().to_string(), y.as_ref().to_string())
    };
    let session_id = DirectSessionId::for_pair(me, &peer);
    let row = direct_store::ensure_session(db, session_id.as_ref(), &a, &b)
        .await
        .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;

    let peer_pid = ParticipantId::for_tagma(&peer);
    Ok(Json(DirectSessionView {
        session_id,
        peer: DirectSessionPeer {
            tagma_id: peer,
            handle: agent_handle(&peer_pid, &peer_profile.owner_username),
        },
        created_at: row.created_at,
    }))
}

/// List the caller's direct sessions, each with the OTHER member's identity.
/// A registry blip degrades one peer's handle (the unforgeable id-prefix) --
/// it never blanks the list.
async fn list_direct_sessions(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<DirectSessionView>>, ApiError> {
    let Some(me) = as_tagma(&principal) else {
        return Err(ApiError::not_found("unknown session"));
    };
    let db = state.require_db()?;
    let rows = direct_store::sessions_for_member(db, me.as_ref())
        .await
        .map_err(map_db_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let peer_tid = TagmaId::from(row.peer_of(me.as_ref()).to_string());
        let peer_pid = ParticipantId::for_tagma(&peer_tid);
        let handle = match crate::control_policy::tagma_profile(&*state.control, &peer_tid).await {
            Ok(Some(p)) => agent_handle(&peer_pid, &p.owner_username),
            _ => degraded_handle(&peer_pid, ParticipantKind::Agent),
        };
        out.push(DirectSessionView {
            session_id: DirectSessionId::from(row.id),
            peer: DirectSessionPeer {
                tagma_id: peer_tid,
                handle,
            },
            created_at: row.created_at,
        });
    }
    Ok(Json(out))
}

/// Send a plaintext `DirectMessage` envelope into the session: the
/// post_room_envelope contract on a two-member membership source. The path
/// is authoritative; the sender must be the authed principal AND a session
/// member; the payload is persisted opaquely and fanned to the peer's live
/// tunnel (an offline peer misses the fan -- the history route is the
/// recovery path). The lesche's synchronous 202 is the ack.
async fn post_direct_session_envelope(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(session_id): Path<String>,
    Json(mut env): Json<Envelope>,
) -> Result<StatusCode, ApiError> {
    let Some(me) = as_tagma(&principal) else {
        return Err(ApiError::not_found("unknown session"));
    };
    // The path is authoritative: a body claiming a different session would
    // otherwise be trusted downstream.
    if env.channel_id.as_ref() != session_id {
        return Err(ApiError::bad_request(
            "envelope channel_id does not match the path",
        ));
    }
    let db = state.require_db()?;
    let row = require_member_session(db, &session_id, me).await?;
    let pid = ParticipantId::for_tagma(me);
    // A member may send only as themselves. A member already knows the
    // session exists, so a 403 here leaks nothing.
    if env.sender.id != pid || env.sender.kind != ParticipantKind::Agent {
        return Err(ApiError::forbidden("envelope sender does not match auth"));
    }
    stamp_sender(&state, &mut env.sender, me, &pid).await;

    // Persist the payload (plaintext `DirectMessage` JSON) opaquely, then fan
    // to the peer. The fan reuses the room machinery over a synthetic
    // two-member snapshot -- the sender-exclusion compares participant ids.
    let payload = env.ciphertext.0.clone();
    direct_store::append(db, &session_id, me.as_ref(), &payload)
        .await
        .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;
    let peer_member = MemberId::from(ParticipantId::for_tagma(&TagmaId::from(
        row.peer_of(me.as_ref()).to_string(),
    )));
    let membership = RoomMembership {
        members: vec![
            RoomMember {
                id: MemberId::from(pid),
                kind: ParticipantKind::Agent,
            },
            RoomMember {
                id: peer_member,
                kind: ParticipantKind::Agent,
            },
        ],
        membership_epoch: 0,
    };
    let _out = {
        let reg = state.read()?;
        fan_envelope(&reg, &membership, &env)
    };
    Ok(StatusCode::ACCEPTED)
}

/// Pull the session's history: `seq > after_seq`, ascending, clamped page.
/// Member-only, same existence-oracle. Each row's sender handle resolves
/// fresh from the registry; a registry blip degrades all handles (the
/// unforgeable id-prefix) and never blanks the pull.
async fn get_direct_session_messages(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(session_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<HistoryQuery>,
) -> Result<Json<Vec<DirectMessageView>>, ApiError> {
    let Some(me) = as_tagma(&principal) else {
        return Err(ApiError::not_found("unknown session"));
    };
    let db = state.require_db()?;
    require_member_session(db, &session_id, me).await?;
    let limit = query
        .limit
        .unwrap_or(HISTORY_MAX_LIMIT)
        .min(HISTORY_MAX_LIMIT);
    let rows = direct_store::read_since(db, &session_id, query.after_seq.unwrap_or(0), limit)
        .await
        .map_err(map_db_err)?;

    // One MemberRef per distinct sender; the member-id is the derived
    // participant id and the source id IS the tagma id (no membership-graph
    // indirection -- direct senders are tagmas, forever).
    let mut refs: Vec<MemberRef> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for r in &rows {
        if seen.insert(r.sender.clone(), ()).is_some() {
            continue;
        }
        let pid = ParticipantId::for_tagma(&TagmaId::from(r.sender.clone()));
        refs.push(MemberRef {
            id: MemberId::from(pid),
            kind: ParticipantKind::Agent,
            source_id: r.sender.clone(),
        });
    }
    let resolved: HashMap<MemberId, _> = match resolve_handles(&*state.control, &refs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "direct history sender resolve failed; degrading handles");
            HashMap::new()
        }
    };
    let views = rows
        .into_iter()
        .map(|r| {
            let pid = ParticipantId::for_tagma(&TagmaId::from(r.sender));
            let member_id = MemberId::from(pid.clone());
            let sender = match resolved.get(&member_id) {
                Some(ri) => kallip_lesche_common::message::Participant {
                    id: pid,
                    kind: ParticipantKind::Agent,
                    handle: ri.handle.clone(),
                    tagma_id: ri.tagma_id.clone(),
                },
                None => kallip_lesche_common::message::Participant {
                    id: pid,
                    kind: ParticipantKind::Agent,
                    handle: degraded_handle(&member_id, ParticipantKind::Agent),
                    tagma_id: None,
                },
            };
            DirectMessageView {
                seq: r.seq,
                sender,
                ciphertext: Ciphertext(r.payload),
                created_at: r.created_at,
            }
        })
        .collect();
    Ok(Json(views))
}

/// Advance the caller's read cursor in the session (the unread watermark).
/// Member-only, same 404 shape; the store's single-statement upsert is
/// clamp-on-write, so a stale write never moves the watermark backwards.
async fn put_direct_read_cursor(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(session_id): Path<String>,
    Json(req): Json<ReadCursorRequest>,
) -> Result<StatusCode, ApiError> {
    if req.last_read_seq < 0 {
        return Err(ApiError::bad_request("last_read_seq must be >= 0"));
    }
    let Some(me) = as_tagma(&principal) else {
        return Err(ApiError::not_found("unknown session"));
    };
    let db = state.require_db()?;
    require_member_session(db, &session_id, me).await?;
    direct_store::set_read_cursor(db, &session_id, me.as_ref(), req.last_read_seq)
        .await
        .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// History query string (the room history route's shape).
#[derive(Debug, Deserialize, Default)]
pub struct HistoryQuery {
    /// Return messages with `seq > after_seq` (exclusive). Default 0 = from
    /// the start.
    pub after_seq: Option<i64>,
    /// Max messages to return. Server-clamped.
    pub limit: Option<u64>,
}

#[cfg(test)]
mod tests;
