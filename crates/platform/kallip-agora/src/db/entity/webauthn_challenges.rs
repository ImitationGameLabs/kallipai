//! `webauthn_challenges` entity — an in-flight WebAuthn ceremony (register or
//! login), bridging the begin/finish split. The opaque `id` is the ceremony id
//! returned to the client; `state` holds the serialised ceremony state, whose
//! type varies by `kind` (see that field's doc). Rows expire after a short TTL
//! and are GC'd.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "webauthn_challenges")]
pub struct Model {
    /// Ceremony id (CSPRNG UUID); returned to the client as the handle to
    /// finish the ceremony. Primary key.
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// Discriminates the `state` JSONB payload. One of the ceremony kinds
    /// defined in `routes/auth.rs` / `routes/passkeys.rs`: `register` (a bare
    /// core `RegistrationState` for a discoverable signup credential),
    /// `add_regular` (a `PasskeyRegistration` for an authenticated non-
    /// discoverable add-passkey), `add_discoverable` (a bare `RegistrationState`
    /// for a discoverable add-passkey), `login` and `login_discoverable` (a
    /// `PasskeyAuthentication`), or `pair` (a `PasskeyRegistration` for device
    /// pairing, which also holds the pairing code hash in `pairing_code_hash`).
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// The serialised ceremony state. Type varies by `kind`: a bare core
    /// `RegistrationState` for `register` and `add_discoverable` (discoverable /
    /// resident-key enrollments), a `PasskeyRegistration` for `add_regular` and
    /// `pair`, and a `PasskeyAuthentication` for the login kinds.
    pub state: Json,
    /// The device-pairing-code hash: for `pair`, held so the finish txn can
    /// re-lock / conditionally consume it. `None` for the register / add / login
    /// kinds (open signup holds no code; login resolves by username). `Vec<u8>`
    /// -> Postgres BYTEA.
    #[sea_orm(nullable)]
    pub pairing_code_hash: Option<Vec<u8>>,
    /// For register: the pre-generated `UserId` that the finish txn will create.
    /// For login: the `UserId` resolved from the username at `begin`. Plain
    /// `TEXT`, NOT a FK (see the migration: at register the user row does not
    /// exist yet).
    #[sea_orm(column_type = "Text", nullable)]
    pub user_id: Option<String>,
    /// For register: the chosen username, carried across the begin/finish split
    /// so finish can insert the `users` row and run the uniqueness check. `None`
    /// for login.
    #[sea_orm(column_type = "Text", nullable)]
    pub username: Option<String>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
