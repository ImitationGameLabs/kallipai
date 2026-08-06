//! Read-only room views: the user-device roster snapshot and the tagma's room
//! discovery poll. Ported verbatim from the agora registry; the creator
//! designation (strict total-order minimum among live Agent members) is a pure
//! function of the moved membership tables, so it carries over unchanged.

use super::shared::{member_of, parse_kind};
use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{room_members, rooms};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::member_identity::{MemberRef, degraded, resolve_handles};
use crate::state::SharedConvState;
use axum::Json;
use axum::extract::{Path, State};
use kallip_agora_common::ids::{MemberId, ParticipantKind, RoomId, TagmaId};
use kallip_agora_common::principal::require_tagma;
use kallip_agora_common::rooms::{RoomMemberProfile, RoomRosterView, TagmaRoomView, Visibility};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, TransactionTrait,
};

/// List the calling tagma's rooms with each room's live membership + whether
/// THIS tagma is the creator. The creator is the strict total-order minimum
/// `(joined_at ASC, member_id ASC)` among the room's live Agent members --
/// a stable, server-authoritative designation. Self-only: the path tagma id
/// must equal the bearer's (the tagma's own room-membership pump).
pub(super) async fn list_tagma_rooms(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(path_tagma_id): Path<String>,
) -> Result<Json<Vec<TagmaRoomView>>, ApiError> {
    let bearer = require_tagma(&principal)?;
    if path_tagma_id != bearer.as_ref() {
        return Err(ApiError::forbidden("a tagma may list only its own rooms"));
    }
    Ok(Json(rooms_for_tagma(&state, bearer).await?))
}

/// The user-side view of the rooms ONE of the caller's tagmata has joined (the
/// owner-management source for the "Manage rooms" dialog). Unlike
/// [`list_tagma_rooms`] (tagma-bearer, self-only), this attests ownership
/// through the agora registry: the caller must be the tagma's owner. A revoked
/// or disabled tagma still has rooms the owner must be able to manage, so the
/// gate is the raw `owner_user_id == caller` (NOT `bilateral_resolvable`); any
/// failure -- unknown / not-owned -- collapses to one "unknown tagma" 404.
pub(super) async fn list_my_tagma_rooms(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(path_tagma_id): Path<String>,
) -> Result<Json<Vec<TagmaRoomView>>, ApiError> {
    let caller = require_user(&principal)?;
    let tagma = TagmaId::from(path_tagma_id);
    let owned = crate::control_policy::tagma_profile(&*state.control, &tagma)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .is_some_and(|p| p.owner_user_id == *caller);
    if !owned {
        return Err(ApiError::not_found("unknown tagma"));
    }
    Ok(Json(rooms_for_tagma(&state, &tagma).await?))
}

/// Build the room list for a tagma: its rooms (newest-joined first), each with
/// its live membership snapshot, membership epoch, visibility, display name,
/// and whether THIS tagma is the room's creator. Shared by the tagma-self poll
/// ([`list_tagma_rooms`]) and the owner view ([`list_my_tagma_rooms`]).
async fn rooms_for_tagma(
    state: &SharedConvState,
    tagma_id: &TagmaId,
) -> Result<Vec<TagmaRoomView>, ApiError> {
    let db = state.require_db()?;
    let self_participant = MemberId::for_tagma(tagma_id);
    // The tagma's rooms, newest-joined first (looked up by source_id since the
    // member_id is derived, not stored as a tagma id).
    let my_rooms = room_members::Entity::find()
        .filter(room_members::Column::SourceId.eq(tagma_id.to_string()))
        .filter(room_members::Column::Kind.eq(ParticipantKind::Agent.as_str()))
        .order_by_desc(room_members::Column::JoinedAt)
        .all(db)
        .await
        .map_err(map_db_err)?;
    let room_ids: Vec<String> = my_rooms.iter().map(|r| r.room_id.clone()).collect();
    if room_ids.is_empty() {
        return Ok(Vec::new());
    }
    // Batch the room rows (epoch) and every member row (membership + creator)
    // for these rooms -- 2 queries total, no N+1. The members are ordered on
    // the composite-PK leading column so the per-room diff is stable across
    // polls regardless of heap/index order.
    let room_rows = rooms::Entity::find()
        .filter(rooms::Column::Id.is_in(room_ids.clone()))
        .all(db)
        .await
        .map_err(map_db_err)?;
    let all_members = room_members::Entity::find()
        .filter(room_members::Column::RoomId.is_in(room_ids))
        .order_by_asc(room_members::Column::MemberId)
        .all(db)
        .await
        .map_err(map_db_err)?;
    let epoch_by_room: std::collections::HashMap<String, i64> = room_rows
        .iter()
        .map(|r| (r.id.clone(), r.membership_epoch))
        .collect();
    let name_by_room: std::collections::HashMap<String, String> = room_rows
        .iter()
        .map(|r| (r.id.clone(), r.name.clone()))
        .collect();
    let visibility_by_room: std::collections::HashMap<String, Visibility> = room_rows
        .iter()
        .map(|r| (r.id.clone(), Visibility::from_db(&r.visibility)))
        .collect();
    let mut members_by_room: std::collections::HashMap<String, Vec<room_members::Model>> =
        std::collections::HashMap::new();
    for m in all_members {
        members_by_room
            .entry(m.room_id.clone())
            .or_default()
            .push(m);
    }
    // Build a view per room, newest-joined first (preserve my_rooms order).
    let mut views = Vec::with_capacity(my_rooms.len());
    for mr in my_rooms {
        let room_id = mr.room_id.clone();
        let members = members_by_room.remove(&room_id).unwrap_or_default();
        // Creator = strict-min (joined_at ASC, member_id ASC) live Agent
        // member. The total order on member_id breaks same-instant ties
        // deterministically.
        let creator = members
            .iter()
            .filter(|m| m.kind == ParticipantKind::Agent.as_str())
            .min_by(|a, b| {
                a.joined_at
                    .cmp(&b.joined_at)
                    .then_with(|| a.member_id.cmp(&b.member_id))
            })
            .map(|m| m.member_id.clone());
        let is_creator = creator.as_deref() == Some(self_participant.as_ref());
        // `room_members.room_id` is ON DELETE CASCADE to `rooms.id`, so
        // every room present in `my_rooms` has a row in `room_rows`. The 0
        // fallback is unreachable today; kept defensive so a future torn
        // CASCADE read degrades to a self-healing skip instead of a 500.
        let membership_epoch = *epoch_by_room.get(&room_id).unwrap_or(&0);
        // Defensive: same CASCADE reasoning as `membership_epoch` -- the row
        // exists, so fall back to Private only for a torn read.
        let visibility = visibility_by_room
            .get(&room_id)
            .copied()
            .unwrap_or(Visibility::Private);
        // `name` falls back to an empty string only for a torn CASCADE read
        // (same reasoning as `membership_epoch`); collapse that to `None` so the
        // wire stays optional and the client renders its own placeholder.
        let name = name_by_room
            .get(&room_id)
            .filter(|s| !s.is_empty())
            .cloned();
        let members = members.iter().map(member_of).collect();
        views.push(TagmaRoomView {
            room_id: RoomId::from(room_id),
            members,
            membership_epoch,
            is_creator,
            visibility,
            name,
        });
    }
    Ok(views)
}

/// Resolve each member's display identity (label + stable handle) via the
/// shared [`resolve_handles`] (agents -> `tagma_profiles`, humans ->
/// `user_identities`; two batched RPCs, no per-member calls, no involvement of
/// the send-path `AgentProfileCache` -- the cache is never evicted on
/// disconnect, so it would give a stale, "online-biased" picture). Members are
/// returned in input order (member_id-ascending) so the client's roster
/// diff stays stable.
///
/// Display policy lives in [`resolve_handles`]: a member the registry resolves
/// gets its real `label` + stable handle, REGARDLESS of authz state (a revoked
/// tagma or a disabled human is still a room member with a name to show);
/// revocation/disability are authz states, not display states. A member the
/// registry does NOT resolve (e.g. a since-deleted account) degrades to a
/// prefix-only handle with no label.
async fn resolve_member_profiles(
    state: &SharedConvState,
    members: &[room_members::Model],
) -> Result<Vec<RoomMemberProfile>, ApiError> {
    let refs: Vec<MemberRef> = members
        .iter()
        .map(|m| MemberRef {
            id: MemberId::from(m.member_id.clone()),
            kind: parse_kind(&m.kind),
            source_id: m.source_id.clone(),
        })
        .collect();
    let resolved = resolve_handles(&*state.control, &refs)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?;
    // Preserve input order (member_id-ascending). `resolve_handles` fills
    // every input ref, so the `unwrap_or_else` degraded fallback is defensive
    // only.
    let mut profiles: Vec<RoomMemberProfile> = members
        .iter()
        .map(|m| {
            let pid = MemberId::from(m.member_id.clone());
            let kind = parse_kind(&m.kind);
            let id = resolved
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| degraded(&pid, kind));
            RoomMemberProfile {
                id: pid,
                kind,
                label: id.label,
                handle: id.handle,
                online: false,
                tagma_id: id.tagma_id,
            }
        })
        .collect();
    // Stamp the live `online` flag from the registry AFTER all the awaits above
    // (lock-discipline invariant #1: no `.await` under a lock). The lookups in
    // `set_online_flags` are synchronous and the guard is dropped at the call's
    // end; a future registry RPC added here MUST stay above this line.
    set_online_flags(state, &mut profiles)?;
    Ok(profiles)
}

/// Set each member's `online` from the live registry: an agent is online iff it
/// holds a tunnel (`presence`), a human iff it holds an app stream
/// (`app_streams`). Both maps are keyed by the derived participant id, so the
/// lookup is uniform across kinds (and across the `MemberId`/`ParticipantId`
/// views of it).
/// uniform across kinds. The roster deliberately reads the live registry here
/// (not the send-path `AgentProfileCache`, which is never evicted on disconnect
/// and would give a stale, online-biased picture).
fn set_online_flags(
    state: &SharedConvState,
    members: &mut [RoomMemberProfile],
) -> Result<(), ApiError> {
    let reg = state.read()?;
    for m in members.iter_mut() {
        m.online = match m.kind {
            ParticipantKind::Agent => reg.presence_by_member(&m.id).is_some(),
            ParticipantKind::Human => reg.app_stream_by_member(&m.id).is_some(),
        };
    }
    Ok(())
}

/// `GET /v1/rooms/{room_id}` -- a single room's live membership snapshot, for a
/// USER who is a current member. A non-member gets 404 (existence is hidden).
/// `is_creator` is server-authoritative: the room's `created_by_user_id`, so it
/// survives a browser refresh.
pub(super) async fn room_roster(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(room_id): Path<String>,
) -> Result<Json<RoomRosterView>, ApiError> {
    let db = state.require_db()?;
    let user_id = require_user(&principal)?;
    let room = RoomId::from(room_id);
    let self_participant = MemberId::for_user(user_id);
    // Membership gate, room row, and member list under ONE REPEATABLE READ
    // snapshot. Postgres's default READ COMMITTED gives each statement its own
    // snapshot, so reading them separately (or even back-to-back in a READ
    // COMMITTED txn) could let a just-removed member pass the gate and still see
    // the roster; REPEATABLE READ freezes a single snapshot for the whole txn,
    // so the gate and the list agree. `None` from the closure is the
    // not-a-member sentinel (the txn returns `TxnError`, not `ApiError`, so the
    // 404 is raised after the closure).
    let snapshot = db
        .transaction::<_, Option<(Option<rooms::Model>, Vec<room_members::Model>)>, TxnError>(
            |txn| {
                let room_str = room.as_ref().to_string();
                let self_pid = self_participant.as_ref().to_string();
                Box::pin(async move {
                    // Must precede any query: sets this txn's isolation before the
                    // first statement takes its snapshot.
                    txn.execute_unprepared("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                        .await?;
                    let membership = room_members::Entity::find()
                        .filter(room_members::Column::RoomId.eq(room_str.clone()))
                        .filter(room_members::Column::MemberId.eq(self_pid))
                        .one(txn)
                        .await?;
                    if membership.is_none() {
                        return Ok(None);
                    }
                    let room_row = rooms::Entity::find()
                        .filter(rooms::Column::Id.eq(room_str.clone()))
                        .one(txn)
                        .await?;
                    let member_rows = room_members::Entity::find()
                        .filter(room_members::Column::RoomId.eq(room_str))
                        .order_by_asc(room_members::Column::MemberId)
                        .all(txn)
                        .await?;
                    Ok(Some((room_row, member_rows)))
                })
            },
        )
        .await;
    let (room_row, member_rows) = match flatten_txn(snapshot)? {
        Some(pair) => pair,
        None => return Err(ApiError::not_found("unknown room")),
    };
    // The caller is a member and `room_members.room_id` is ON DELETE
    // CASCADE to `rooms.id`, so the room row exists; the 0 fallback is
    // defensive (a torn CASCADE read degrades to a self-healing skip, not a
    // 500). Real epochs start at 1.
    let membership_epoch = room_row.as_ref().map(|r| r.membership_epoch).unwrap_or(0);
    let visibility = room_row
        .as_ref()
        .map(|r| Visibility::from_db(&r.visibility))
        .unwrap_or(Visibility::Private);
    let is_creator = room_row
        .map(|r| r.created_by_user_id == user_id.as_ref())
        .unwrap_or(false);
    let members = resolve_member_profiles(&state, &member_rows).await?;
    Ok(Json(RoomRosterView {
        room_id: room,
        members,
        membership_epoch,
        is_creator,
        visibility,
    }))
}
