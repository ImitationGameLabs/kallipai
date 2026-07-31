//! `webauthn_challenges` entity — an in-flight WebAuthn ceremony (register or
//! login), bridging the begin/finish split. The opaque `id` is the ceremony id
//! returned to the client; `state` holds the serialised `PasskeyRegistration`
//! or `PasskeyAuthentication`. Rows expire after a short TTL and are GC'd.

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
    /// `'register'` or `'login'` — discriminates the `state` JSONB payload.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// The serialised ceremony state (`PasskeyRegistration` for register,
    /// `PasskeyAuthentication` for login).
    pub state: Json,
    /// The held redeem-code hash: for `register`, the invite-code hash; for
    /// `pair`, the device-pairing-code hash; held so the finish txn can re-lock
    /// / conditionally consume it. `None` for login. `Vec<u8>` -> Postgres BYTEA.
    /// (The DB column keeps its historical name `invite_code_hash` via the
    /// `column_name` alias below; the Rust field is self-documenting.)
    #[sea_orm(column_name = "invite_code_hash", nullable)]
    pub held_code_hash: Option<Vec<u8>>,
    /// For register: the pre-generated `UserId` that the finish txn will create.
    /// For login: the `UserId` resolved from the email at `begin`. Plain
    /// `TEXT`, NOT a FK (see the migration: at register the user row does not
    /// exist yet).
    #[sea_orm(column_type = "Text", nullable)]
    pub user_id: Option<String>,
    /// For register: the canonicalized login email, carried across the
    /// begin/finish split so finish can insert the `users` row and run the
    /// uniqueness check. `None` for login.
    #[sea_orm(column_type = "Text", nullable)]
    pub email: Option<String>,
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
