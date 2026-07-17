//! `passkeys` entity — a registered WebAuthn credential bound to a user.
//!
//! The high-level wrapper `Passkey` is stored in the `credential` JSONB column
//! (the `webauthn-rs` documented storage model), with `cred_id` mirrored as a
//! `UNIQUE` column so the login ceremony can resolve
//! `credential id -> stored passkey` without a scan. Backup flags live inside
//! the JSONB (not mirrored): `Passkey` only exposes them behind
//! `danger-credential-internals`, and nothing queries them by column today.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "passkeys")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// `UserId` of the owner. References `users(id)`.
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    /// WebAuthn credential id (authenticator-supplied). Globally unique;
    /// indexed for the login lookup. `Vec<u8>` -> Postgres `BYTEA`.
    pub cred_id: Vec<u8>,
    /// The full `webauthn_rs::prelude::Passkey`, serialised to JSON. Carries the
    /// COSE public key, signature counter, backup flags, transports.
    pub credential: Json,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// Set when `webauthn-rs` reports a signature-counter regression (possible
    /// clone). A compromised passkey is filtered out of `login_begin` and
    /// cannot authenticate; the user must re-register. `None` = live.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub compromised_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
