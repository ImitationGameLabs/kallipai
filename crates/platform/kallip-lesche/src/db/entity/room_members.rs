//! `room_members` entity -- live membership of a room.
//!
//! One row per (room, member). `member_id` is the opaque derived conversation
//! identity (`ParticipantId::for_user` / `for_tagma` -- a member is a participant
//! who belongs to the room); `kind` is `"human"` / `"agent"`; `source_id` is the
//! underlying `user_id` / `tagma_id` (a plain string, NOT a foreign key -- the
//! underlying tables live in the agora registry). Composite PK `(room_id,
//! member_id)`; no status column. A participant is a member iff a row exists.
//! Removal hard-deletes the row and appends a `room_member_revocations` audit
//! entry (the live/revocation-audit split).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "room_members")]
pub struct Model {
    /// `RoomId`. Composite-PK half; references `rooms(id)` (`ON DELETE CASCADE`).
    #[sea_orm(primary_key, column_type = "Text")]
    pub room_id: String,
    /// `ParticipantId` (opaque derived). Composite-PK half. NOT a foreign key.
    #[sea_orm(primary_key, column_type = "Text")]
    pub member_id: String,
    /// `"human"` / `"agent"` -- the `ParticipantKind` wire label.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// The underlying `user_id` (Human) or `tagma_id` (Agent). A plain string
    /// (NOT a FK) so the membership row is self-contained and the boundary
    /// stays clean.
    #[sea_orm(column_type = "Text")]
    pub source_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub joined_at: OffsetDateTime,
    /// The `user_id` that added this member. Plain string (audit fact).
    #[sea_orm(column_type = "Text")]
    pub added_by: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
