//! `external_identities` entity -- a linked OAuth (GitHub, Google) account.
//!
//! A `(provider, subject)` pair bound to a `users` row. An account may have
//! 0..N of these AND 0..N WebAuthn passkeys; either kind can be the sole
//! sign-in method. Unlink is a hard-delete (no revocations/audit table: an
//! OAuth `(provider, subject)` legitimately re-links after unlink, so a
//! denylist would either block that or be a dead row -- the audit channel is
//! structured logs, unlike passkey `cred_id`s which must refuse re-binding).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "external_identities")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    /// `UserId` of the owner. References `users(id)` with cascading delete.
    #[sea_orm(column_type = "Text")]
    pub user_id: String,
    /// Stable provider discriminator: `"github"` | `"google"`. Never rename.
    #[sea_orm(column_type = "Text")]
    pub provider: String,
    /// The provider's stable account id (GitHub numeric `id`, Google `sub`).
    /// The login resolution key alongside `provider`.
    #[sea_orm(column_type = "Text")]
    pub subject: String,
    /// Best-effort display name from the provider (display only; never used for
    /// account merge or as a login key).
    #[sea_orm(column_type = "Text", nullable)]
    pub display_name: Option<String>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// Stamped on every sign-in via this identity; `None` until first use.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub last_used_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
