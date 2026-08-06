//! `room_invites` entity -- the pending invite/accept admission flow.
//!
//! A row is an offer from `invited_by_user_id` to `invitee_user_id` to join
//! `room_id`. `accepted_at` is `None` while pending; on accept, a
//! `room_members` row is inserted, the membership epoch is bumped, and
//! `accepted_at` is stamped. `expires_at` bounds the offer. The user-id columns
//! are plain TEXT references, NOT foreign keys -- the `users` table lives in
//! the agora registry, not in lesche's store.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "room_invites")]
pub struct Model {
    /// Synthetic invite id (never crosses an id-newtype boundary).
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// `RoomId`. References `rooms(id)` (`ON DELETE CASCADE`).
    #[sea_orm(column_type = "Text")]
    pub room_id: String,
    /// `UserId` of the invitee. Plain TEXT reference (no FK): the `users` table
    /// lives in the agora registry.
    #[sea_orm(column_type = "Text")]
    pub invitee_user_id: String,
    /// `UserId` of the inviter (the offer's author). Plain TEXT reference.
    #[sea_orm(column_type = "Text")]
    pub invited_by_user_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
    /// `None` while pending; stamped on accept (which inserts the `room_members`
    /// row). NULL therefore means "pending" without a
    /// status column.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub accepted_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
