//! The `proxy_keys` entity: hash-only proxy key rows.

use kallip_common::authtoken::TokenKind;
use sea_orm::entity::prelude::*;

/// The proxy-key token family: the minted bearer prefix (a crate-local
/// closed set, the `TokenKind` convention the other services follow).
pub(crate) const PROXY_KEY: TokenKind = TokenKind("sk-proxy-");

/// The presented bearer is SHA-256 hashed (`TokenHash`); only the 32-byte
/// hash is stored (Postgres `BYTEA`). A key's allowed sets live in
/// `proxy_key_sets`. Issuance, revocation, and expiry are the key admin
/// face's lifecycle (`/admin/keys`); these rows and `key_lifecycle_events`
/// are written only by that face. The plaintext is shown once, in the
/// mint response, and never stored.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "proxy_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key_hash: Vec<u8>,
    /// Owning tagma identity. Informational (the authorization
    /// matrix keys off it).
    pub tagma_id: String,
    /// Issuance time (explicit on every write).
    pub created_at: time::OffsetDateTime,
    /// When the key stops resolving (the caller answers a lapsed key
    /// with the dedicated `key_expired` code). Expiry is a derived state
    /// read at resolution time, never a lifecycle row of its own.
    /// `None` = never expires.
    pub expires_at: Option<time::OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
