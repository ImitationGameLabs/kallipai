//! `device_pairing_codes` entity — a short-lived, single-use, account-scoped
//! pairing code minted by an authenticated device so a new device can enroll
//! its own passkey onto an existing account (cross-device bootstrap). The TTL
//! lives in `routes::device_pairing::PAIR_CODE_TTL`.
//!
//! Only the SHA-256 hash of the code is stored (`code_hash` PK); the plaintext
//! is returned once at mint. `consumed_at` is `None` until the redeem flow
//! finishes (conditional `UPDATE ... WHERE consumed_at IS NULL` is the
//! anti-double-enroll mutex). `user_id` is bound at mint by the authenticated
//! caller — it is the account the new passkey will be enrolled onto.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "device_pairing_codes")]
pub struct Model {
    /// SHA-256 hash of the pairing code; primary key. `Vec<u8>` -> Postgres BYTEA.
    #[sea_orm(primary_key)]
    pub code_hash: Vec<u8>,
    /// `UserId` whose account this code authorizes a passkey enrollment onto.
    /// References `users(id)` (cascading delete). Bound at mint by the caller.
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
    /// Set when the redeem flow finishes; `None` = redeemable. The conditional
    /// update on this column is the anti-double-enroll mutex.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub consumed_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
