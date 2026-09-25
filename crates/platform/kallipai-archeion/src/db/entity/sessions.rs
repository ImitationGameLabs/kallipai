//! `sessions` entity — an opaque cookie session. The cookie value is a random
//! `sk-sess-...` token; only its SHA-256 hash is stored. Row deletion = logout
//! / revocation.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions")]
pub struct Model {
    /// SHA-256 hash of the `sk-sess-...` cookie value; primary key. `Vec<u8>` ->
    /// Postgres `BYTEA`.
    #[sea_orm(primary_key)]
    pub token_hash: Vec<u8>,
    /// `UserId` of the session owner. References `users(id)`.
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
    /// Freshness marker for the one-shot step-up gating "add a passkey". Set to
    /// `now()` by login/register finish; consumed (set `NULL`) by the
    /// add-passkey begin txn. `NULL` = consumed or never freshly authed. Nullable
    /// on purpose: the absence of a pending step-up is real state, not a sentinel.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub authed_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
