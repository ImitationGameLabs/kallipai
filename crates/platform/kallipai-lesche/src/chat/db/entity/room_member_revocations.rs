//! `room_member_revocations` entity -- append-only audit of removed room
//! members.
//!
//! Mirrors the live/revocation-audit split: a removal hard-deletes the
//! `room_members` live row and appends one row here. `member_id` / `source_id`
//! are plain strings (NOT foreign keys) so the audit fact survives the
//! subject's deletion. `room_id` is `ON DELETE CASCADE` to `rooms`. The
//! synthetic `id` is a UUID that never crosses an id-newtype boundary.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "room_member_revocations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Uuid")]
    pub id: Uuid,
    /// References `rooms(id)` (`ON DELETE CASCADE`).
    #[sea_orm(column_type = "Text")]
    pub room_id: String,
    /// The removed member's `ParticipantId` (plain string).
    #[sea_orm(column_type = "Text")]
    pub member_id: String,
    /// `"human"` / `"agent"`.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// The underlying `user_id` / `tagma_id` (plain string).
    #[sea_orm(column_type = "Text")]
    pub source_id: String,
    /// The `user_id` that revoked. Plain string (audit fact, must survive the
    /// revoker's own deletion).
    #[sea_orm(column_type = "Text")]
    pub revoked_by: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub revoked_at: OffsetDateTime,
    #[sea_orm(column_type = "Text")]
    pub reason: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
