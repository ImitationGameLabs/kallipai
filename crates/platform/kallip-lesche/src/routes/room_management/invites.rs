//! The invite flow: an inbox of pending invites, invite creation (any member
//! may invite), and accept (invitee-only). Both creation and accept run the
//! membership write + epoch bump in one transaction. Ported from the agora
//! registry; `create_invite` attests the invitee's existence through the agora
//! registry rather than a local users-table read. A real membership change on
//! accept fires a local post-commit fan (`spawn_local_membership_fan`); a
//! re-accept by an already-member is a no-op and does not.

use std::collections::HashMap;

use super::shared::{
    bump_epoch_locked, enforce_member_cap_locked, require_member_locked, spawn_local_membership_fan,
};
use crate::auth::{AuthPrincipal, require_user};
use crate::db::entity::{room_invites, room_members, rooms};
use crate::db::{TxnError, flatten_txn, map_db_err};
use crate::identity::{degraded_handle, human_handle};
use crate::state::SharedConvState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kallip_agora_common::ids::{ParticipantId, ParticipantKind, RoomId, UserId};
use kallip_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, SqlErr, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Default invite TTL. A pending invite expires after this window if not
/// accepted; the accept handler rejects an expired or already-accepted invite
/// with 409.
const INVITE_TTL: time::Duration = time::Duration::days(7);

#[derive(Serialize)]
pub(super) struct InviteView {
    pub(super) invite_id: Uuid,
    pub(super) room_id: String,
    /// The inviter's @handle (resolved from the durable `invited_by_user_id`),
    /// so the inbox never exposes a raw user id. A since-deleted inviter
    /// degrades to a prefix handle.
    pub(super) invited_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(super) created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub(super) expires_at: OffsetDateTime,
}

/// List the caller's pending invites (the invitee's inbox), newest first. Only
/// unaccepted, unexpired invites: this is the set the caller can still accept.
pub(super) async fn list_my_invites(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Json<Vec<InviteView>>, ApiError> {
    let user_id = require_user(&principal)?;
    let db = state.require_db()?;
    let rows = room_invites::Entity::find()
        .filter(room_invites::Column::InviteeUserId.eq(user_id.to_string()))
        .filter(room_invites::Column::AcceptedAt.is_null())
        .filter(room_invites::Column::ExpiresAt.gt(OffsetDateTime::now_utc()))
        .order_by_desc(room_invites::Column::CreatedAt)
        .all(db)
        .await
        .map_err(map_db_err)?;
    // Resolve each inviter's @handle from the durable `invited_by_user_id` (the
    // row stores only the id; the handle is derived, matching the roster). Only
    // when there is at least one invite, so an empty inbox does not trigger an
    // RPC on every poll. Duplicate inviter ids are harmless (the bulk reader
    // returns one row per user; the result map dedups). A disabled-but-real
    // inviter still resolves (render their @username); a since-deleted inviter
    // misses and degrades to a prefix handle.
    let inviter_handles: HashMap<String, String> = if rows.is_empty() {
        HashMap::new()
    } else {
        let inviter_ids: Vec<UserId> = rows
            .iter()
            .map(|r| UserId::from(r.invited_by_user_id.clone()))
            .collect();
        state
            .control
            .user_identities(&inviter_ids)
            .await
            .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
            .into_iter()
            .map(|u| (u.user_id.to_string(), human_handle(&u.username)))
            .collect()
    };
    let items = rows
        .into_iter()
        .map(|r| {
            let inviter_id = r.invited_by_user_id.clone();
            let invited_by = inviter_handles
                .get(&inviter_id)
                .cloned()
                .unwrap_or_else(|| degraded_handle(&inviter_id, ParticipantKind::Human));
            InviteView {
                invite_id: r.id,
                room_id: r.room_id,
                invited_by,
                created_at: r.created_at,
                expires_at: r.expires_at,
            }
        })
        .collect();
    Ok(Json(items))
}

#[derive(Deserialize)]
pub(super) struct CreateInviteRequest {
    pub(super) invitee_username: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CreateInviteResponse {
    pub(super) invite_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub(super) expires_at: OffsetDateTime,
}

/// Invite a user to a room. Any current member may invite. The invitee must be a
/// real user account (attested by the agora registry). At most one unaccepted
/// invite per (room, invitee) is allowed: a still-live prior unaccepted invite
/// is a 409; an expired one is deleted to re-open the slot (lazy GC). The
/// check+delete+insert run in one transaction with the prior row locked, so a
/// concurrent duplicate serializes; the partial unique index backstops any
/// residual race and maps to the same 409.
pub(super) async fn create_invite(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(room_id): Path<String>,
    Json(req): Json<CreateInviteRequest>,
) -> Result<(StatusCode, Json<CreateInviteResponse>), ApiError> {
    let db = state.require_db()?;
    let inviter = require_user(&principal)?;
    let room = RoomId::from(room_id);
    // Invite by @handle: strip the sigil + surrounding whitespace, then resolve
    // to the canonical user through the registry. Unknown / disabled / malformed
    // handles all collapse to one 404 "unknown user" so the reason is not
    // leaked (the existence-oracle invariant). The durable invite is still keyed
    // by the resolved user_id below.
    let handle = req
        .invitee_username
        .trim()
        .trim_start_matches('@')
        .to_string();
    let invitee = state
        .control
        .user_identity_by_username(&handle)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .filter(|u| !u.disabled)
        .map(|u| u.user_id)
        .ok_or_else(|| ApiError::not_found("unknown user"))?;
    let now = OffsetDateTime::now_utc();
    let expires_at = now + INVITE_TTL;
    let invite_id = Uuid::new_v4();
    let result = db
        .transaction::<_, _, TxnError>(|txn| {
            let room = room.to_string();
            let invitee = invitee.to_string();
            let inviter = inviter.to_string();
            Box::pin(async move {
                // Membership authz inside the txn: the inviter must be a current
                // member at insert time, closing the window where a removal
                // between an outside-txn gate and the insert would let a
                // just-removed member create a pending invite.
                require_member_locked(txn, &room, &inviter).await?;
                // Lock any prior unaccepted invite for this (room, invitee) so
                // two concurrent creates serialize.
                let prior = room_invites::Entity::find()
                    .filter(room_invites::Column::RoomId.eq(room.clone()))
                    .filter(room_invites::Column::InviteeUserId.eq(invitee.clone()))
                    .filter(room_invites::Column::AcceptedAt.is_null())
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                if let Some(prior) = prior {
                    if prior.expires_at > OffsetDateTime::now_utc() {
                        return Err(TxnError::Api(ApiError::conflict("invite already pending")));
                    }
                    // Expired: free the slot so a fresh invite can be issued.
                    room_invites::Entity::delete_by_id(prior.id)
                        .exec(txn)
                        .await?;
                }
                room_invites::ActiveModel {
                    id: Set(invite_id),
                    room_id: Set(room),
                    invitee_user_id: Set(invitee),
                    invited_by_user_id: Set(inviter),
                    created_at: Set(now),
                    expires_at: Set(expires_at),
                    accepted_at: Set(None),
                }
                .insert(txn)
                .await?;
                Ok(())
            })
        })
        .await;
    // A unique-violation here is the residual race past the row lock (or a
    // same-txn insert collision); map it to the same 409 the live-prior path
    // returns rather than a 500.
    match result {
        Ok(()) => {}
        Err(sea_orm::TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(sea_orm::TransactionError::Transaction(TxnError::Db(e)))
        | Err(sea_orm::TransactionError::Connection(e)) => {
            if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) {
                return Err(ApiError::conflict("invite already pending"));
            }
            return Err(map_db_err(e));
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(CreateInviteResponse {
            invite_id,
            expires_at,
        }),
    ))
}

/// Accept a pending invite. Invitee-only: the caller must be the invitee. On
/// accept: insert the `room_members` row (idempotent if already a member),
/// stamp the invite `accepted_at`, and bump the room epoch -- all in one
/// transaction. An expired or already-accepted invite is 409. No identity RPC:
/// the caller's existence is implied by their verified session.
pub(super) async fn accept_invite(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path((room_id, invite_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let db = state.require_db()?;
    let caller = require_user(&principal)?;
    let room = RoomId::from(room_id);
    let invite_uuid: Uuid = invite_id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid invite id"))?;
    let participant_id = ParticipantId::for_user(caller);
    let result = db
        .transaction::<_, _, TxnError>(|txn| {
            let room = room.to_string();
            let caller = caller.to_string();
            let participant_id = participant_id.as_ref().to_string();
            Box::pin(async move {
                // Lock the invite row FOR UPDATE so a double-accept race
                // serializes: the second txn sees accepted_at set.
                let invite = room_invites::Entity::find_by_id(invite_uuid)
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::not_found("unknown invite")))?;
                if invite.invitee_user_id != caller {
                    // Existence-oracle: a non-invitee gets the same 404.
                    return Err(TxnError::Api(ApiError::not_found("unknown invite")));
                }
                if invite.accepted_at.is_some() {
                    return Err(TxnError::Api(ApiError::conflict("invite already accepted")));
                }
                if invite.expires_at <= OffsetDateTime::now_utc() {
                    return Err(TxnError::Api(ApiError::conflict("invite expired")));
                }
                if invite.room_id != room {
                    return Err(TxnError::Api(ApiError::not_found("unknown invite")));
                }
                let now = OffsetDateTime::now_utc();

                // Insert the membership if not already present (a user re-joining
                // a room they are still in is a no-op member write but still
                // stamps the invite). A check-then-insert under the room lock is
                // race-free here.
                let room_row = rooms::Entity::find_by_id(room.clone())
                    .lock_exclusive()
                    .one(txn)
                    .await?
                    .ok_or_else(|| TxnError::Api(ApiError::not_found("unknown room")))?;
                let already_member = room_members::Entity::find()
                    .filter(room_members::Column::RoomId.eq(room.clone()))
                    .filter(room_members::Column::MemberId.eq(participant_id.clone()))
                    .one(txn)
                    .await?
                    .is_some();
                if !already_member {
                    enforce_member_cap_locked(txn, &room).await?;
                    room_members::ActiveModel {
                        room_id: Set(room.clone()),
                        member_id: Set(participant_id),
                        kind: Set(ParticipantKind::Human.as_str().to_string()),
                        source_id: Set(caller.clone()),
                        joined_at: Set(now),
                        added_by: Set(caller),
                    }
                    .insert(txn)
                    .await?;
                }
                // Stamp the invite accepted.
                let mut am: room_invites::ActiveModel = invite.into();
                am.accepted_at = Set(Some(now));
                am.update(txn).await?;
                // Bump the epoch only when membership actually changed -- a
                // re-accept by an already-member is a no-op member write and must
                // not invalidate every member's roster cache.
                if !already_member {
                    bump_epoch_locked(txn, &room_row).await?;
                }
                Ok(!already_member)
            })
        })
        .await;
    // The closure returns whether membership changed; fan only on a real add
    // (a re-accept by an already-member is a no-op that must not invalidate
    // every member's reconcile state).
    let joined_newly = flatten_txn(result)?;
    if joined_newly {
        spawn_local_membership_fan(state.clone(), room.clone());
    }
    Ok(StatusCode::NO_CONTENT)
}
