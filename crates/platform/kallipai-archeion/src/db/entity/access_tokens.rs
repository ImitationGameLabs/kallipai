//! `access_tokens` entity — one credential record, for a tagma or a user's
//! personal access token. Keyed by its SHA-256 hash; the record id outlives the
//! secret (a reset swaps the hash, keeps the row and its audit trail).
//!
//! `owner_kind` says which id `owner_id` holds: `tagma` (a tagma's single
//! credential, enforced by a partial unique index) or `user` (one of the
//! user's several personal tokens). `perms` is the stored permission set and
//! travels to the data planes as stored: a record whose owner is gone or
//! revoked stops resolving, so verification drops the whole credential rather
//! than narrowing the set.
//!
//! `masked` is NULL for a record whose plaintext predates this table: the
//! migration carries a tagma's existing hash across without inventing a tail
//! it never saw.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "access_tokens")]
pub struct Model {
    #[sea_orm(primary_key)]
    #[sea_orm(column_type = "Text")]
    pub id: String,
    /// `tagma` or `user`.
    #[sea_orm(column_type = "Text")]
    pub owner_kind: String,
    /// The owning `tagmata.id` or `users.id`, per `owner_kind`.
    #[sea_orm(column_type = "Text")]
    pub owner_id: String,
    /// The human-given name ("cli-laptop", "ci", "enrollment").
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// SHA-256 of the plaintext. Never the plaintext.
    pub token_hash: Vec<u8>,
    /// Display tail (`sk-pat-…ab12`); NULL when the plaintext is unknown.
    #[sea_orm(column_type = "Text")]
    pub masked: Option<String>,
    /// The stored permission set, as a JSON array of strings.
    pub perms: Json,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// The user who created the record (the tagma's owner for an enrollment).
    #[sea_orm(column_type = "Text")]
    pub created_by: String,
    /// `None` = never expires (a tagma's credential).
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: Option<OffsetDateTime>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub last_used_at: Option<OffsetDateTime>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
