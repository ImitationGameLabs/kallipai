//! Cross-handler helpers shared across the room-management submodules: the
//! member-row -> view mapping, the in-txn membership gate, and the epoch bump.
//! Ported from the agora registry; identity is now attested through the agora
//! `/internal/*` surface rather than local table reads, but the membership
//! graph itself lives here.

use crate::db::TxnError;
use crate::db::entity::{room_members, rooms};
use crate::fan::deliver_membership_changed;
use crate::state::SharedConvState;
use kallip_agora_common::ids::{MemberId, ParticipantKind, RoomId, UserId};
use kallip_agora_common::participant::RoomMember;
use kallip_common::protocol::ApiError;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter};

/// Hard cap on members per room. Deliberately small: a room is a
/// human/agent collaboration group, not a mass chat, and coordination
/// efficiency collapses past a certain size. Raise only when a real need
/// appears.
pub(super) const MAX_ROOM_MEMBERS: u64 = 30;

/// Reject with 409 "room is full" when the room is already at the member cap.
/// Race-free only because the caller holds the room row `lock_exclusive()` --
/// every membership-add path takes that lock before its check-then-insert, so
/// this count is atomic with the subsequent insert. Place the call AFTER the
/// idempotency check so a re-add of an already-present member stays a no-op.
pub(super) async fn enforce_member_cap_locked(
    txn: &impl ConnectionTrait,
    room: &str,
) -> Result<(), TxnError> {
    let count = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room))
        .count(txn)
        .await?;
    if count >= MAX_ROOM_MEMBERS {
        return Err(TxnError::Api(ApiError::conflict("room is full")));
    }
    Ok(())
}

/// Parse a stored `kind` string into a [`ParticipantKind`]. Defensive: any
/// unknown value is treated as `Agent` (the historical default for non-user
/// members); the column is written only by this crate's own handlers, so a
/// clean parse is the common path.
pub(super) fn parse_kind(s: &str) -> ParticipantKind {
    ParticipantKind::from_label(s).unwrap_or(ParticipantKind::Agent)
}

/// Build a [`RoomMember`] from a live membership row.
pub(super) fn member_of(row: &room_members::Model) -> RoomMember {
    RoomMember {
        id: MemberId::from(row.member_id.clone()),
        kind: parse_kind(&row.kind),
    }
}

/// 404 if `user` (by raw id string) is not a current member of `room`, inside a
/// txn. When the caller subsequently mutates membership or bumps the epoch, it
/// must take the room-row `lock_exclusive()` first so the check-then-act is
/// atomic (see `add_tagma` / `accept_invite`); for an authz-only read inside a
/// txn (e.g. `create_invite`) the row lock is not required, so under READ
/// COMMITTED a concurrent removal can still land between this check and the
/// insert commit (acceptable "member at check time" semantics).
pub(super) async fn require_member_locked(
    txn: &impl ConnectionTrait,
    room: &str,
    user: &str,
) -> Result<(), TxnError> {
    let member_id = MemberId::for_user(&UserId::from(user.to_string()));
    let member = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room))
        .filter(room_members::Column::MemberId.eq(member_id.as_ref().to_string()))
        .one(txn)
        .await?;
    if member.is_none() {
        return Err(TxnError::Api(ApiError::not_found("unknown room")));
    }
    Ok(())
}

/// Advance the room's membership epoch by one. The caller has the room row
/// locked FOR UPDATE, so the read of `room_row.membership_epoch` + the write are
/// atomic. The epoch is the membership-version counter the relay and its clients
/// key on.
pub(super) async fn bump_epoch_locked(
    txn: &impl ConnectionTrait,
    room_row: &rooms::Model,
) -> Result<(), sea_orm::DbErr> {
    rooms::Entity::update_many()
        .filter(rooms::Column::Id.eq(room_row.id.clone()))
        .col_expr(
            rooms::Column::MembershipEpoch,
            sea_orm::sea_query::Expr::value(room_row.membership_epoch + 1),
        )
        .exec(txn)
        .await?;
    Ok(())
}

/// After a room's `membership_epoch` bumps (a committed membership mutation),
/// notify the room's online members to reconcile. This is the local replacement
/// for the agora->lesche wake push: a single relay instance fans directly to its
/// in-process `Registry` (no HTTP hop). Best-effort and off the request path:
/// the task runs on a detached `tokio::spawn`, a FRESH post-commit
/// `room_members` SELECT loads the live members (the just-removed member is
/// already excluded once the txn committed), and `deliver_membership_changed`
/// runs under the registry read lock with no await beneath it. Offline members
/// are skipped (they reconcile on reconnect); the mutation's epoch bump is the
/// durable signal that backstops a lost fan.
pub(super) fn spawn_local_membership_fan(state: SharedConvState, room: RoomId) {
    let Some(db) = state.db.clone() else {
        return;
    };
    tokio::spawn(async move {
        let rows = match room_members::Entity::find()
            .filter(room_members::Column::RoomId.eq(room.to_string()))
            .all(&db)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "membership fan: live-member query failed");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        let members: Vec<RoomMember> = rows.iter().map(member_of).collect();
        // Registry poison is swallowed: presence/membership fan is best-effort
        // soft state (the epoch bump + roster poll resync), so a 500 here would
        // be wrong -- but log it so a poisoning event stays diagnosable.
        let Ok(reg) = state.read() else {
            tracing::warn!("membership fan: registry lock poisoned");
            return;
        };
        deliver_membership_changed(&reg, &room, &members);
    });
}
