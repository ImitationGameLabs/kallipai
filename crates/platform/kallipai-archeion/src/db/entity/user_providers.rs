//! `user_providers` entity -- one saved AI provider credential (API key,
//! optional base-URL override) owned by an account.
//!
//! The mixed-mode vault: each row carries its own `mode`. With
//! `mode = "plaintext"` the `key_material` column holds the raw key; with
//! `mode = "encrypted"` it holds a client-encrypted blob. The server treats
//! `key_material` as opaque in BOTH modes -- encrypt/decrypt work happens in
//! web client, so archeion never interprets the column (the OAuth-session
//! read-only degradation falls out of this: a session without the device key
//! can list rows but an encrypted key is unusable off-device).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "user_providers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// `UserId` of the owner. References `users(id)` with cascading delete.
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    /// User-visible label for the entry. Unique per account
    /// (`idx_user_providers_user_name`).
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// Provider discriminator (e.g. `"anthropic"`). Metadata, stored in the
    /// clear so the library view can filter without decryption.
    #[sea_orm(column_type = "Text")]
    pub provider: String,
    /// Optional endpoint override for the provider's built-in default.
    #[sea_orm(column_type = "Text", nullable)]
    pub base_url: Option<String>,
    /// The key itself: plaintext key when `mode = "plaintext"`, client-side
    /// encrypted blob when `mode = "encrypted"`. Opaque to the server.
    #[sea_orm(column_type = "Text")]
    pub key_material: String,
    /// `"plaintext"` | `"encrypted"` (TEXT + handler validation -- no enum
    /// column precedent in archeion).
    #[sea_orm(column_type = "Text")]
    pub mode: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// Set by the handlers on every mutation (no ActiveModelBehavior hook
    /// precedent in this crate).
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
