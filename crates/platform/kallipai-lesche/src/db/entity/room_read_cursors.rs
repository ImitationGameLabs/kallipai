//! `room_read_cursors` entity -- a member's per-room read watermark (the
//! unread backbone).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "room_read_cursors")]
pub struct Model {
    /// `RoomId`. Composite-PK half; references `rooms(id)` (`ON DELETE CASCADE`).
    #[sea_orm(primary_key, column_type = "Text")]
    pub room_id: String,
    /// `ParticipantId` (opaque derived). Composite-PK half. NOT a foreign key,
    /// mirroring `room_members`.
    #[sea_orm(primary_key, column_type = "Text")]
    pub member_id: String,
    /// The member's read watermark: every message with `seq <= last_read_seq`
    /// is read. Writes clamp (never move backwards) -- the store's upsert
    /// owns that invariant.
    pub last_read_seq: i64,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
