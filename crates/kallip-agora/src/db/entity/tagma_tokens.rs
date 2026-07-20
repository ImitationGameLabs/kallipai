//! `tagma_tokens` entity — a herald's long-lived bearer (`sk-tagma-...`), keyed
//! by its SHA-256 hash. Revocation lives on the owning `tagmata` row
//! (`tagmata.revoked_at`), the single source of truth, rather than here.

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
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
