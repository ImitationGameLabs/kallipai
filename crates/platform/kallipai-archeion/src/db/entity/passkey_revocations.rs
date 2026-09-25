//! `passkey_revocations` entity — append-only audit log of revoked WebAuthn
//! credentials. The live credential is hard-deleted from `passkeys` at the same
//! time a row is appended here, so the two tables never overlap: `passkeys` is
//! live-only, this table is history-only.
//!
//! `cred_id` is indexed so it doubles as a denylist at add-passkey finish
//! (re-binding a previously-revoked credential id is refused). `reason` /
//! `revoked_by` are plain `TEXT` value sets (constants below) rather than a
//! sea-orm enum, so adding a new reason later is non-breaking.
//!
//! Clone detection (signature-counter regression) does NOT write here: the
//! signal is too noisy (synced passkeys report counter 0; firmware quirks) and
//! auto-revoking on it is an attacker-triggerable DoS. A regression only denies
//! the single login + logs; it never mutates the credential.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

/// Revocation reason. Today only user/operator-initiated revoke; the constant
/// set is extensible without a schema change.
pub const REASON_REVOKED: &str = "revoked";

/// Who triggered the revocation.
pub const REVOKED_BY_USER: &str = "user";
pub const REVOKED_BY_ADMIN: &str = "admin";

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "passkey_revocations")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// `UserId` of the owner. References `users(id)` (cascading delete).
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    /// The revoked credential id. Indexed (denylist lookup at add-passkey
    /// finish). `Vec<u8>` -> Postgres `BYTEA`.
    pub cred_id: Vec<u8>,
    /// One of the `REASON_*` constants. Plain TEXT, not an enum.
    #[sea_orm(column_type = "Text")]
    pub reason: String,
    /// One of the `REVOKED_BY_*` constants. Plain TEXT, not an enum.
    #[sea_orm(column_type = "Text")]
    pub revoked_by: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub revoked_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
