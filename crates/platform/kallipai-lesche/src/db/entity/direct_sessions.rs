//! `direct_sessions` entity -- one server-side plaintext 1v1 session between
//! exactly two tagma agents.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "direct_sessions")]
pub struct Model {
    /// The derived v4-form session id (`DirectSessionId`). Primary key: the
    /// pair-to-id derivation makes one session per member pair.
    #[sea_orm(primary_key, column_type = "Text")]
    pub id: String,
    /// Canonical byte-ordered member pair (`member_a` <= `member_b`, the same
    /// ordering the id derivation hashes). Plain TEXT tagma-id references,
    /// NOT FKs to the archeion registry (same boundary as
    /// `room_members.member_id`).
    #[sea_orm(column_type = "Text")]
    pub member_a: String,
    #[sea_orm(column_type = "Text")]
    pub member_b: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
