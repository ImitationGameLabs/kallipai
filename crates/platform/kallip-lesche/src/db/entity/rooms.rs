//! `rooms` entity -- a persistent multi-member chat room.
//!
//! A room is M users + N agents (the membership graph lives in `room_members`).
//! `created_by_user_id` is the creator; it is a plain
//! TEXT reference, NOT a foreign key, because the `users` table lives in the
//! agora registry (lesche never touches the registry store). `membership_epoch`
//! is the membership-version counter the relay reads to authorize senders,
//! bumped on every add/remove.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "rooms")]
pub struct Model {
    /// `RoomId` (opaque UUID-string newtype), stored as `TEXT`. A fresh UUID for
    /// multi-member rooms.
    #[sea_orm(primary_key, column_type = "Text")]
    pub id: String,
    /// `UserId` of the creator. Plain TEXT reference (no FK): the `users` table
    /// lives in the agora registry, not in lesche's store.
    #[sea_orm(column_type = "Text")]
    pub created_by_user_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// Membership-version counter. Starts at 1; bumped on every member add or
    /// remove. The relay authorizes senders against the live membership and
    /// surfaces the version to clients on roster read.
    pub membership_epoch: i64,
    /// Human-readable room label. Required at create (the route rejects an empty
    /// name); display-only metadata, immutable after create like `visibility`.
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// Optional longer room description. Display-only metadata, immutable after
    /// create; the empty string means "no description".
    #[sea_orm(column_type = "Text")]
    pub description: String,
    /// Room visibility, stored as the `Visibility::as_str` label (`private` or
    /// `public`). Immutable after create. Validated to `Visibility` on read via
    /// `Visibility::from_db`; an unknown value degrades to `Private`. See
    /// [`kallip_agora_common::rooms::Visibility`].
    #[sea_orm(column_type = "Text")]
    pub visibility: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
