//! `users` entity — a human account, created at invite redemption. The id is a
//! `UserId` (opaque UUID-string newtype) stored as `TEXT`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    /// `UserId` (opaque UUID-string newtype), stored as `TEXT`.
    #[sea_orm(primary_key, column_type = "Text")]
    pub id: String,
    /// In-site display handle, normalized at write time (trim + ASCII-lowercase,
    /// `[a-z0-9_-]{3,32}`). Unique; NOT the login id.
    #[sea_orm(column_type = "Text")]
    pub username: String,
    /// Login handle, canonicalized at write time per RFC 5321 sec 2.4 (local
    /// part preserved verbatim, domain lowercased). Unique; resolved at
    /// `login_begin`.
    #[sea_orm(column_type = "Text")]
    pub email: String,
    /// Optional human-readable name, NULL until set.
    #[sea_orm(column_type = "Text", nullable)]
    pub display_name: Option<String>,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub created_at: OffsetDateTime,
    /// Set when an admin disables the account (no disable endpoint is wired
    /// yet); `None` = active. A disabled user cannot log in, use a session, or
    /// enroll.
    #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
    pub disabled_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
