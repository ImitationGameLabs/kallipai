//! The room-keyed envelope surface: `POST /v1/rooms/{room_id}/envelopes`.
//!
//! An envelope addressed to a room is:
//!
//! 1. **Authorized** -- the sender (a `Participant`) must be the authenticated
//!    principal AND a current member of the room (served from the membership
//!    cache). A non-member or unknown room is a uniform 404.
//! 2. **Persisted** -- the payload is appended to the durable `room_messages`
//!    store so offline members pull it on reconnect (skipped when the relay runs
//!    without a store, i.e. mock tests). Room payloads are plaintext
//!    `RoomMessage` JSON, stored opaquely -- the lesche is the room's store of
//!    record and the server is trusted to read room content.
//! 3. **Fanned** -- delivered to every other member via [`crate::fan::fan_envelope`]
//!    (user members' app streams, tagma members' tunnels).

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::ParticipantKind;
use kallip_agora_common::principal::Principal;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::message::{Envelope, Participant};
use kallip_lesche_common::rooms::{MemberId, RoomId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::auth::{AuthPrincipal, OptUserDisplay};
use crate::db::map_db_err;
use crate::db::store;
use crate::fan::fan_envelope;
use crate::identity::{agent_handle, degraded_handle, human_handle};
use crate::member_identity::{MemberRef, degraded, resolve_handles};
use crate::state::{AgentProfile, SharedConvState};

/// Is `(mid, kind)` a current member of `membership`? Both fields are matched:
/// the v5 user/tagma id namespaces are disjoint today, but the kind check is
/// cheap defense-in-depth against any future id-collision and keeps the single
/// AuthZ gate uniform across callers.
fn is_member(
    membership: &kallip_lesche_common::rooms::RoomMembership,
    mid: &MemberId,
    kind: ParticipantKind,
) -> bool {
    membership
        .members
        .iter()
        .any(|m| m.id == *mid && m.kind == kind)
}

pub fn router() -> Router<SharedConvState> {
    Router::new()
        .route("/rooms/{room_id}/envelopes", post(post_room_envelope))
        .route("/rooms/{room_id}/messages", get(room_history))
}

async fn post_room_envelope(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    OptUserDisplay(user_display): OptUserDisplay,
    Path(room_id): Path<String>,
    Json(mut env): Json<Envelope>,
) -> Result<StatusCode, ApiError> {
    let room = RoomId::from(room_id);
    // The path is authoritative: a body claiming a different room would
    // otherwise be trusted downstream.
    if env.channel_id.as_ref() != room.as_ref() {
        return Err(ApiError::bad_request(
            "envelope channel_id does not match the path",
        ));
    }

    // Membership is read from the lesche-local graph (one SQL read, strongly
    // consistent with mutations in this DB). Unknown room -> 404.
    let db = state.require_db()?;
    let membership = store::room_membership(db, room.as_ref())
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown room"))?;

    // Authorize: the sender must be the authed principal (id + kind) AND a room
    // member. Existence-oracle: a non-member (and an Admin, who is never a room
    // participant) gets the same 404 as an unknown room. The membership gate runs
    // BEFORE the sender-match, so a spoofed sender cannot be used to distinguish
    // a real from a nonexistent room (a non-member probing with another member's
    // id gets 404, not a 403 that confirms the room exists). The room surface
    // deals only in participant ids, so the underlying user/tagma id never
    // appears here.
    let (authed_pid, authed_kind) = match (principal.participant_id(), principal.participant_kind())
    {
        (Some(pid), Some(kind)) => (pid, kind),
        _ => return Err(ApiError::not_found("unknown room")), // Admin
    };
    // `authed_pid` is the wire `ParticipantId` (used below for the profile cache
    // + handle stamping); the membership gate works in the room-domain twin.
    let authed_mid = MemberId::from(authed_pid.clone());
    if !is_member(&membership, &authed_mid, authed_kind) {
        return Err(ApiError::not_found("unknown room"));
    }
    // A member may send only as themselves. A member already knows the room
    // exists, so a 403 here (not a 404) leaks nothing.
    if env.sender.id != authed_pid || env.sender.kind != authed_kind {
        return Err(ApiError::forbidden("envelope sender does not match auth"));
    }

    // Stamp the authoritative sender identity before persist + fan-out, retiring
    // the advisory client-supplied `Participant.handle`. The relay is the sole
    // source of room identity: a tagma cannot self-declare its label/owner (the
    // profile is registry-resolved), and a human's name comes from the verified
    // session. Built once here so the durable row and the live event agree.
    env.sender.handle = match &principal {
        Principal::Tagma(tid) => match state.agent_profiles.get(&authed_pid) {
            Some(p) => agent_handle(&authed_pid, &p.owner_username),
            None => match crate::control_policy::tagma_profile(&*state.control, tid).await {
                Ok(Some(p)) if crate::control_policy::tunnel_usable(&p) => {
                    let profile = AgentProfile {
                        label: p.label,
                        owner_username: p.owner_username,
                        owner_display_name: p.owner_display_name,
                    };
                    let h = agent_handle(&authed_pid, &profile.owner_username);
                    state.agent_profiles.set(authed_pid.clone(), profile);
                    h
                }
                // Unknown / not usable (pending, revoked, no pinned key) / a
                // transient registry error: degrade to the unforgeable id-prefix
                // only -- never the tagma-supplied handle, and never a 500 (a
                // registry outage must not fail the send). `degraded_handle` is
                // shared with the history-read fallback, so the vocabulary has
                // one home.
                _ => degraded_handle(&authed_pid, ParticipantKind::Agent),
            },
        },
        Principal::User(_) => match &user_display {
            // Stable handle = the user's login @username (NOT their mutable
            // display name, which is prepended at render).
            Some(d) => human_handle(&d.username),
            None => degraded_handle(&authed_pid, ParticipantKind::Human),
        },
        Principal::Admin => unreachable!("admin is not a room participant"),
    };

    // The agent's tagma_id rides the live envelope (and the history read re-derives it)
    // so a message header can deep-link to that tagma's profile without reversing the
    // one-way participant id. Humans carry none.
    env.sender.tagma_id = match &principal {
        Principal::Tagma(tid) => Some(tid.clone()),
        _ => None,
    };

    // Persist the room payload (plaintext `RoomMessage` JSON), stored opaquely
    // as bytes. The lesche is the room's store of record and is trusted to read
    // room content. Only the sender's stable identity (`id` + `kind`) is
    // persisted; the stamped `handle` rides the live fan-out below and is
    // re-derived from the registry on history read, so no display handle is
    // frozen into the row.
    store::append(
        db,
        room.as_ref(),
        &authed_mid,
        env.sender.kind,
        membership.membership_epoch,
        &env.ciphertext.0,
    )
    .await
    .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;

    // Fan to every other member under the registry read lock. The `membership`
    // snapshot is read once above (no lock on the room row), so a member removed
    // by a concurrent mutation in this window may receive this one extra live
    // envelope -- self-healing (the durable row is correct and the next fan-out
    // excludes them); not worth a row lock on the send hot path.
    let _out = {
        let reg = state.read()?;
        fan_envelope(&reg, &membership, &env)
    };

    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize, Default)]
struct HistoryQuery {
    /// Return messages with `seq > after_seq` (exclusive). Default 0 = from the
    /// start.
    after_seq: Option<i64>,
    /// Max messages to return. Server-clamped.
    limit: Option<u64>,
}

/// Server cap on a single history page, defending an unbounded client `limit`.
const HISTORY_MAX_LIMIT: u64 = 200;

#[derive(Debug, Serialize)]
struct StoredMessageView {
    seq: i64,
    sender: Participant,
    epoch: i64,
    ciphertext: Ciphertext,
    #[serde(with = "time::serde::iso8601")]
    created_at: OffsetDateTime,
}

/// Pull a room's message history. Member-only (user or tagma principal must
/// belong to the room). Returns stored rows whose payload is the plaintext
/// `RoomMessage` JSON (the lesche is the room's store of record and is trusted
/// to read room content).
async fn room_history(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(room_id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Vec<StoredMessageView>>, ApiError> {
    let room = RoomId::from(room_id);
    let db = state.require_db()?;
    let membership = store::room_membership(db, room.as_ref())
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown room"))?;
    // Member-only: existence-oracle 404 for non-members. The room surface deals
    // only in participant ids + kinds.
    let (authed_mid, authed_kind) = match (
        principal.participant_id().map(MemberId::from),
        principal.participant_kind(),
    ) {
        (Some(mid), Some(kind)) => (mid, kind),
        _ => return Err(ApiError::not_found("unknown room")), // Admin
    };
    if !is_member(&membership, &authed_mid, authed_kind) {
        return Err(ApiError::not_found("unknown room"));
    }

    let limit = q.limit.unwrap_or(HISTORY_MAX_LIMIT).min(HISTORY_MAX_LIMIT);
    let rows = store::read_since(db, room.as_ref(), q.after_seq.unwrap_or(0), limit)
        .await
        .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;
    let views = resolve_history_views(&state, db, room.as_ref(), rows).await?;
    Ok(Json(views))
}

/// Resolve each history row's sender display handle fresh from the registry,
/// then assemble the wire views. The row carries only the sender's stable
/// `ParticipantId` + kind; the handle is derived here (consistent with the
/// roster, which uses the same `resolve_handles`).
///
/// Source-id recovery: a sender's `user_id`/`tagma_id` (what the registry
/// resolves by) is fetched from live `room_members` OR the
/// `room_member_revocations` audit -- a sender who left after sending is
/// gone from the live table but retained in the audit, so departed senders keep
/// their real `@owner` handle. A sender in NEITHER (exceptional data gap), or a
/// registry RPC failure, degrades to the unforgeable `<kind> <short_prefix>`
/// fallback -- a registry blip never blanks a history pull.
async fn resolve_history_views(
    state: &SharedConvState,
    db: &crate::db::Db,
    room: &str,
    rows: Vec<store::StoredMessage>,
) -> Result<Vec<StoredMessageView>, ApiError> {
    use std::collections::HashMap;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Distinct senders + each sender's kind (a member has one kind).
    let mut kind_by_mid: HashMap<String, ParticipantKind> = HashMap::new();
    for r in &rows {
        kind_by_mid
            .entry(r.sender_id.as_ref().to_string())
            .or_insert(r.sender_kind);
    }
    let distinct_mids: Vec<String> = {
        let mut s: Vec<String> = kind_by_mid.keys().cloned().collect();
        s.sort();
        s
    };

    let source_map = store::member_source_map(db, room, &distinct_mids)
        .await
        .map_err(map_db_err)?;

    // Build refs only for senders we can resolve to a source id; senders in
    // neither table (an exceptional data gap) are omitted and degrade via the
    // `unwrap_or_else` in the assembly below.
    let refs: Vec<MemberRef> = distinct_mids
        .iter()
        .filter_map(|mid_s| {
            // `kind_by_mid` is built from `distinct_mids`, so the key is always
            // present; the unwrap_or is defensive only.
            let kind = kind_by_mid
                .get(mid_s)
                .copied()
                .unwrap_or(ParticipantKind::Agent);
            source_map.get(mid_s).map(|source_id| MemberRef {
                id: MemberId::from(mid_s.clone()),
                kind,
                source_id: source_id.clone(),
            })
        })
        .collect();

    // Resolve; on a registry error degrade every sender (return an empty map so
    // the assembly falls back per-sender) rather than failing the history read.
    let resolved: HashMap<MemberId, _> = match resolve_handles(&*state.control, &refs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "history sender resolve failed; degrading handles");
            HashMap::new()
        }
    };

    let views = rows
        .into_iter()
        .map(|r| {
            let id = resolved
                .get(&r.sender_id)
                .cloned()
                .unwrap_or_else(|| degraded(&r.sender_id, r.sender_kind));
            StoredMessageView {
                seq: r.seq,
                sender: Participant {
                    id: r.sender_id.into(),
                    kind: r.sender_kind,
                    handle: id.handle,
                    tagma_id: id.tagma_id,
                },
                epoch: r.epoch,
                ciphertext: Ciphertext(r.ciphertext),
                created_at: r.created_at,
            }
        })
        .collect();
    Ok(views)
}

#[cfg(test)]
mod tests;
