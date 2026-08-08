//! `passkeys` entity — a registered WebAuthn credential bound to a user.
//!
//! This table holds ONLY live (active) credentials. Revoked or clone-detected
//! credentials are hard-deleted and recorded in `passkey_revocations` (an
//! append-only audit table), so every query here is filter-free.
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
    /// User-supplied device label ("iPhone", "MacBook") so a user can tell
    /// their passkeys apart. May be empty for legacy rows; the UI renders a
    /// fallback. NOT NULL with no DB default — every insert supplies a value.
    #[sea_orm(column_type = "Text")]
    pub label: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// When this passkey was last used. Seeded to the enrollment instant (a
    /// registration/pair ceremony is itself a user-verification event) and
    /// stamped on every `login_finish`.
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub last_used_at: OffsetDateTime,
    /// Whether this credential was enrolled via the discoverable (resident-key)
    /// registration flow. A server-side fact (the RP asked for
    /// `require_resident_key(true)`), not an authenticator claim -- it gates the
    /// "passwordless sign-in" UI affordance. Discoverable passkeys participate in
    /// conditional-UI autofill; legacy ones do not (the wrapper registers with
    /// `require_resident_key=false`). Both still work via username login.
    pub discoverable: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
