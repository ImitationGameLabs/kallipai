//! `invite_codes` entity — an admin-minted, single-use invite (`sk-invite-`)
//! redeemed on the web to create a user account + bind a passkey.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "invite_codes")]
pub struct Model {
    /// SHA-256 hash of the `sk-invite-...` plaintext; primary key. `Vec<u8>` ->
    /// Postgres `BYTEA`.
    #[sea_orm(primary_key)]
    pub code_hash: Vec<u8>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
    /// Redemption timestamp; `None` = redeemable.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub consumed_at: Option<OffsetDateTime>,
    /// `UserId` that redeemed this code. References `users(id)`.
    #[sea_orm(column_type = "Text", nullable)]
    pub consumed_by: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub note: Option<String>,
    /// Operator revocation timestamp; `None` = live.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
