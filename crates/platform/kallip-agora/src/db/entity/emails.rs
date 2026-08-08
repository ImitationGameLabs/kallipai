//! `emails` entity -- email as an optional contact/recovery channel.
//!
//! Email is decoupled from login (which resolves by `users.username`) and from
//! the WebAuthn `user.id` (the opaque `UserId`). An account may have several
//! addresses; at most one is primary (enforced by the partial unique index
//! `uniq_emails_primary_per_account`). `verified_at` is `None` until the user
//! completes a verification flow; only verified addresses back account recovery.
//!
//! `address` is globally unique (`uniq_emails_address`) -- one account per
//! canonical address, mirroring the old single-email unique constraint. It is
//! canonicalized at write time per RFC 5321 sec 2.4 (local part verbatim,
//! domain lowercased; see `crate::email`).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "emails")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// `UserId` of the owner. References `users(id)` (cascading delete).
    #[sea_orm(column_type = "Text")]
    pub account_id: String,
    /// Canonicalized address (local part verbatim, domain lowercased).
    #[sea_orm(column_type = "Text")]
    pub address: String,
    /// Whether this is the account's primary contact address. At most one per
    /// account (partial unique index).
    pub is_primary: bool,
    /// Set when the user completes a verification flow; `None` until then.
    /// Only verified addresses back account recovery.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub verified_at: Option<OffsetDateTime>,
    /// SHA-256 hash of the pending verification token, present between an
    /// add/resend and a successful verify. `None` once verified or when no
    /// verification is pending. `Vec<u8>` -> Postgres `BYTEA`.
    pub verification_token_hash: Option<Vec<u8>>,
    /// When the pending verification token expires. Set at `add_email` time;
    /// cleared (NULL) once verified. `verify_email` rejects once it has passed
    /// (as the same generic 404 it uses for unknown/already-consumed tokens).
    /// NULL on backfilled rows (no pending token).
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub verification_token_expires_at: Option<OffsetDateTime>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub added_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
