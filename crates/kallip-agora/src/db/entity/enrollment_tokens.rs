//! `enrollment_tokens` entity — a single-use tagma-access credential
//! (`sk-enroll-...`), redeemed by a herald at enroll to mint a tagma token.
//! Today the admin mints these; a planned self-service surface
//! (`/v1/me/enrollment-tokens`) will let users mint their own. The table is
//! identical either way.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "enrollment_tokens")]
pub struct Model {
    /// Public row id (for the DELETE/list path). A synthetic UUID that never
    /// crosses the id-newtype boundary.
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// SHA-256 hash of the `sk-enroll-...` plaintext; unique. `Vec<u8>` maps to
    /// Postgres `BYTEA`.
    pub token_hash: Vec<u8>,
    /// `UserId` of the minting owner. References `users(id)`.
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
    /// Redemption timestamp; `None` = redeemable.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub consumed_at: Option<OffsetDateTime>,
    /// The tagma a herald enrolled with this token. References `tagmata(id)`.
    #[sea_orm(column_type = "Text", nullable)]
    pub consumed_by_tagma: Option<String>,
    /// Owner revocation timestamp; `None` = live.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
