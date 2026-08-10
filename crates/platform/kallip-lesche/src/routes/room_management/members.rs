//! Membership mutation: adding a tagma to a room, joining a public room, and
//! removing a member. Removals hard-delete the live row, append a
//! revocation-audit row, and bump the epoch in one transaction. Ported from the
//! agora registry; `add_tagma` attests enrollment through the agora
//! `/internal/*` surface rather than a local tagma-table read. Each actual
//! membership change fires a local post-commit fan (`spawn_local_membership_fan`);
//! an idempotent no-op add does not. A real new add past the member cap
//! ([`super::shared::MAX_ROOM_MEMBERS`]) is refused with `409 "room is full"`,
//! checked inside the txn after the idempotency gate.
//!
//! ## Removal authorization
//!
//! `remove_member` is keyed by the opaque derived `member_id` (the `ParticipantId`
//! every room surface already carries), and admits exactly three
//! actors, each of which implies its own right to act -- so there is no separate
//! membership gate:
//! - **self** -- a member removing their own row (leave). Any member may leave.
//! - **owner-of-agent** -- the owner of a tagma member may pull it out of ANY
//!   room, even one the owner is not a member of (the owner controls their agent
//!   everywhere). Ownership is attested through the agora registry
//!   (`tagma_profile`), as a raw `owner_user_id == caller` compare -- NOT
//!   `bilateral_resolvable`, so a revoked/disabled tagma can still be pulled.
//!   This leaks nothing: an owner already discovers their tagma's rooms via
//!   `GET /v1/tagmata/{id}/rooms`.
//! - **creator** -- the room's `created_by_user_id` may remove any other member.
//!
//! Every other case collapses to one `404 "unknown room"` (the existence
//! oracle: a probe learns neither the room nor the target nor the reason).

use super::shared::{
    bump_epoch_locked, enforce_member_cap_locked, require_member_locked, spawn_local_membership_fan,
};
use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{room_member_revocations, room_members, rooms};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::state::SharedConvState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kallip_agora_common::ids::{ParticipantId, ParticipantKind, RoomId, TagmaId, UserId};
use kallip_agora_common::rooms::Visibility;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QuerySelect,
    TransactionTrait,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Deserialize)]
pub(super) struct AddTagmaRequest {
    pub(super) tagma_id: String,
}

/// Add a tagma to a room (the whole tagma, by `tagma_id`). Any current USER
/// member may add (a tagma member authenticates via tunnel, not the HTTP cookie
/// this route requires). The tagma must be enrolled and non-revoked -- attested
/// through the
/// agora registry (a transient agora failure surfaces as 500 before the txn, so
/// no half-commit). Inserting a tagma already in the room is a true no-op (no
/// epoch bump); otherwise insert + bump the epoch, in one txn. The agent-free
/// boundary is preserved: the member is stored as a derived `member_id`,
/// with the `tagma_id` carried as a plain `source_id` string.
pub(super) async fn add_tagma(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(room_id): Path<String>,
    Json(req): Json<AddTagmaRequest>,
) -> Result<StatusCode, ApiError> {
    let caller = require_user(&principal)?;
    let db = state.require_db()?;
    let room = RoomId::from(room_id);
    let tagma = TagmaId::from(req.tagma_id);
    // Usability is derived from the registry's raw tagma facts, attested by the
    // agora registry (the tagmata table lives there). The predicate (enrolled +
    // non-revoked + owner-not-disabled) runs before the txn, so an agora outage
    // is a 500 with no half-commit. Any failure -- unknown / pending / revoked /
    // owner-disabled -- collapses to one "unknown tagma" 404.
    let joinable = crate::control_policy::tagma_profile(&*state.control, &tagma)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .as_ref()
        .is_some_and(crate::control_policy::room_joinable);
    if !joinable {
        return Err(ApiError::not_found("unknown tagma"));
    }
    let participant_id = ParticipantId::for_tagma(&tagma);
    let result = db
        .transaction::<_, _, TxnError>(|txn| {
            let room = room.to_string();
            let tagma = tagma.to_string();
            let caller = caller.to_string();
            let participant_id = participant_id.as_ref().to_string();
            Box::pin(async move {
                let room_row = rooms::Entity::find_by_id(room.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::not_found("unknown room")))?;
                require_member_locked(txn, &room, &caller).await?;
                // Idempotent: a tagma already in the room is a no-op (no epoch
                // bump).
                let already = room_members::Entity::find()
                    .filter(room_members::Column::RoomId.eq(room.clone()))
                    .filter(room_members::Column::MemberId.eq(participant_id.clone()))
                    .one(txn)
                    .await?;
                if already.is_none() {
                    enforce_member_cap_locked(txn, &room).await?;
                    room_members::ActiveModel {
                        room_id: Set(room),
                        member_id: Set(participant_id),
                        kind: Set(ParticipantKind::Agent.as_str().to_string()),
                        source_id: Set(tagma),
                        joined_at: Set(OffsetDateTime::now_utc()),
                        added_by: Set(caller),
                    }
                    .insert(txn)
                    .await?;
                    bump_epoch_locked(txn, &room_row).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
        })
        .await;
    let added = flatten_txn(result)?;
    if added {
        spawn_local_membership_fan(state.clone(), room.clone());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Join a PUBLIC room without an invite (the open-access path). Any authenticated
/// user. A private or unknown room is rejected with the same 404 (use the invite
/// flow for a private room; collapsing the two avoids leaking room existence).
/// Idempotent: a caller already in the room is a no-op (no epoch bump). On a
/// real new membership, insert the member row + bump the epoch in one txn, then
/// fire the local post-commit fan so existing members' browsers reconcile their
/// rosters. Mirrors `accept_invite`'s txn shape without the invite-row
/// bookkeeping.
pub(super) async fn join_public_room(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(room_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let caller = require_user(&principal)?;
    let db = state.require_db()?;
    let room = RoomId::from(room_id);
    let participant_id = ParticipantId::for_user(caller);
    let result = db
        .transaction::<_, _, TxnError>(|txn| {
            let room = room.to_string();
            let caller = caller.to_string();
            let participant_id = participant_id.as_ref().to_string();
            Box::pin(async move {
                let room_row = rooms::Entity::find_by_id(room.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::not_found("unknown room")))?;
                if Visibility::from_db(&room_row.visibility) != Visibility::Public {
                    // Collapse to the same 404 as an unknown room: distinguishing
                    // "private" from "missing" would leak room existence to a
                    // non-member probing ids (existence-oracle rule). Private rooms
                    // are entered through the invite flow instead.
                    return Err(TxnError::Api(ApiError::not_found("unknown room")));
                }
                // Idempotent: already a member -> no epoch bump, no fan.
                let already = room_members::Entity::find()
                    .filter(room_members::Column::RoomId.eq(room.clone()))
                    .filter(room_members::Column::MemberId.eq(participant_id.clone()))
                    .one(txn)
                    .await?;
                if already.is_some() {
                    return Ok(false);
                }
                enforce_member_cap_locked(txn, &room).await?;
                let now = OffsetDateTime::now_utc();
                room_members::ActiveModel {
                    room_id: Set(room),
                    member_id: Set(participant_id),
                    kind: Set(ParticipantKind::Human.as_str().to_string()),
                    source_id: Set(caller.clone()),
                    joined_at: Set(now),
                    // Self-join: the caller is their own sponsor.
                    added_by: Set(caller),
                }
                .insert(txn)
                .await?;
                bump_epoch_locked(txn, &room_row).await?;
                Ok(true)
            })
        })
        .await;
    let joined_newly = flatten_txn(result)?;
    if joined_newly {
        spawn_local_membership_fan(state.clone(), room);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a member (human or tagma) from a room, keyed by the opaque derived
/// `member_id`. Authorization is one of three actors (see the module docs):
/// self (leave), the tagma's owner (cross-room pull), or the room creator.
/// Everything else collapses to `404 "unknown room"`.
///
/// The handler reads `kind` + `source_id` from the target row itself, so the
/// audit record is self-describing and the caller never passes the raw
/// user/tagma id (the member id is the only member identifier on the wire).
///
/// The owner attestation runs BEFORE the txn (same shape as `add_tagma`: a
/// registry error surfaces as 500 with no half-commit). The in-txn authorization
/// decision MUST use the in-txn re-read of `kind`/`member_id`; the pre-txn
/// `is_owner` flag is reused only because `member_id` is a deterministic
/// derivation of `source_id`, so the same `member_id` binds the pre-txn
/// owner fact to the in-txn row. Do NOT "optimize" by reusing the pre-txn kind
/// for the authz branch. The only TOCTOU is "row deleted before the txn" -- the
/// in-txn re-read returns `None` and collapses to the 404.
pub(super) async fn remove_member(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path((room_id, member_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let caller = require_user(&principal)?;
    let db = state.require_db()?;
    let room = RoomId::from(room_id);
    let target_pid = ParticipantId::from(member_id);

    // Pre-txn: learn the target's kind + source_id so an agent target can be
    // attested against its owner. If the row is already gone, collapse to the
    // same 404 the txn would raise (stable code, existence oracle).
    let pre = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.as_ref().to_string()))
        .filter(room_members::Column::MemberId.eq(target_pid.as_ref().to_string()))
        .one(db)
        .await
        .map_err(map_db_err)?;
    let is_owner = match &pre {
        Some(row) if row.kind == ParticipantKind::Agent.as_str() => {
            let tagma = TagmaId::from(row.source_id.clone());
            crate::control_policy::tagma_profile(&*state.control, &tagma)
                .await
                .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
                .is_some_and(|p| p.owner_user_id == *caller)
        }
        _ => false,
    };

    let result = db
        .transaction::<_, _, TxnError>(|txn| {
            let room = room.as_ref().to_string();
            let target_pid = target_pid.as_ref().to_string();
            let caller = caller.as_ref().to_string();
            Box::pin(async move {
                let room_row = rooms::Entity::find_by_id(room.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::not_found("unknown room")))?;
                // Authoritative re-read of the target row. The authz decision
                // below uses THIS row's kind/member_id, not the pre-txn
                // snapshot (see the handler doc invariant).
                let row = room_members::Entity::find()
                    .filter(room_members::Column::RoomId.eq(room.clone()))
                    .filter(room_members::Column::MemberId.eq(target_pid.clone()))
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::not_found("unknown room")))?;
                let self_pid = ParticipantId::for_user(&UserId::from(caller.clone()));
                let is_self = row.kind == ParticipantKind::Human.as_str()
                    && row.member_id == *self_pid.as_ref();
                let is_creator = room_row.created_by_user_id == caller;
                // The pre-txn `is_owner` attestation already required an Agent
                // target; re-bind it to the in-txn `row.kind` so the authz
                // decision literally uses the authoritative re-read (the
                // handler doc's invariant), not the pre-txn snapshot.
                let is_owner = is_owner && row.kind == ParticipantKind::Agent.as_str();
                if !(is_self || is_owner || is_creator) {
                    return Err(TxnError::Api(ApiError::not_found("unknown room")));
                }
                let now = OffsetDateTime::now_utc();
                room_members::Entity::delete_many()
                    .filter(room_members::Column::RoomId.eq(room.clone()))
                    .filter(room_members::Column::MemberId.eq(target_pid.clone()))
                    .exec(txn)
                    .await?;
                room_member_revocations::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    room_id: Set(room),
                    member_id: Set(row.member_id.clone()),
                    kind: Set(row.kind.clone()),
                    source_id: Set(row.source_id.clone()),
                    revoked_by: Set(caller),
                    revoked_at: Set(now),
                    reason: Set("removed".to_string()),
                }
                .insert(txn)
                .await?;
                bump_epoch_locked(txn, &room_row).await?;
                Ok(())
            })
        })
        .await;
    flatten_txn(result)?;
    spawn_local_membership_fan(state.clone(), room.clone());
    Ok(StatusCode::NO_CONTENT)
}
