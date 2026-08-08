//! `oauth_states` entity -- the Authorization-Code ceremony state held between
//! the OAuth `begin` and `finish` handlers. One row per in-flight ceremony,
//! GC'd by the 60s sweep on `expires_at`.
//!
//! PK is the SHA-256 of the `state` CSRF token plaintext (the plaintext rides
//! the provider redirect URL; only its hash is stored, looked up at finish).
//! Kept in a dedicated table rather than `webauthn_challenges` because OAuth
//! state is genuinely non-WebAuthn (no crypto ceremony state, different
//! fields).
//!
//! Lifecycle: `state` is single-use -- a signin ceremony is deleted on finish
//! for a linked identity (login), and for an UNLINKED identity it is
//! transitioned exactly once to "held": `finish` resolves the provider
//! identity and stores it on the row (`subject`, `claim_display_name`) plus a
//! single-use `signup_token_hash`, then returns the opaque token so the SPA
//! can submit a chosen username at `POST /auth/oauth/signup/complete`, which
//! creates the account and deletes the row. A second `finish` on a held row
//! is rejected (401) -- the `state` is no longer redeemable. A link ceremony
//! is deleted on finish like a login.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

/// The ceremony action: `"signin"` (login, or begin signup for an unlinked
/// identity) or `"link"` (bind a provider to the already-signed-in account,
/// set `user_id`).
pub const ACTION_SIGNIN: &str = "signin";
pub const ACTION_LINK: &str = "link";

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_states")]
pub struct Model {
    /// SHA-256 of the `state` plaintext; primary key.
    #[sea_orm(primary_key)]
    pub state_hash: Vec<u8>,
    #[sea_orm(column_type = "Text")]
    pub provider: String,
    /// `ACTION_SIGNIN` | `ACTION_LINK`.
    #[sea_orm(column_type = "Text")]
    pub action: String,
    /// Sanitized relative return path for the SPA callback.
    #[sea_orm(column_type = "Text", nullable)]
    pub return_path: Option<String>,
    /// Set only for `ACTION_LINK`: the account to bind the identity to.
    /// FK-free (mirrors `webauthn_challenges.user_id`).
    #[sea_orm(column_type = "Text", nullable)]
    pub user_id: Option<String>,
    /// PKCE code_verifier (providers that support PKCE); threaded back at
    /// finish. `None` for providers without PKCE.
    #[sea_orm(column_type = "Text", nullable)]
    pub pkce_verifier: Option<String>,
    /// Held only by a signin row transitioned to "held" (unlinked identity):
    /// the resolved provider subject to bind at signup completion. NULL for
    /// login/link and until finish resolves the claim.
    #[sea_orm(column_type = "Text", nullable)]
    pub subject: Option<String>,
    /// Held display name resolved from the provider (display-only; lands on
    /// `external_identities.display_name` at completion). NULL unless held.
    #[sea_orm(column_type = "Text", nullable)]
    pub claim_display_name: Option<String>,
    /// SHA-256 of the opaque `sk-oauthsu-` signup token returned to the SPA.
    /// NULL unless the row is held awaiting a username; the complete endpoint
    /// looks rows up by this hash (partial index).
    pub signup_token_hash: Option<Vec<u8>>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub expires_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
