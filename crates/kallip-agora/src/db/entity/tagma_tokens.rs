//! `tagma_tokens` entity — a herald's long-lived bearer (`sk-tagma-...`), keyed
//! by its SHA-256 hash.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tagma_tokens")]
pub struct Model {
    /// SHA-256 hash of the `sk-tagma-...` plaintext; the primary key (never
    /// store the plaintext). `Vec<u8>` maps to Postgres `BYTEA`.
    #[sea_orm(primary_key)]
    pub token_hash: Vec<u8>,
    /// `TagmaId` this token authenticates. References `tagmata(id)`.
    #[sea_orm(column_type = "Text")]
    pub tagma_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub issued_at: OffsetDateTime,
    /// Revocation timestamp; `None` = live. A revoke endpoint is not yet
    /// implemented; until it exists `resolve_bearer` checks this on every
    /// request so revocation takes effect immediately once wired.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
