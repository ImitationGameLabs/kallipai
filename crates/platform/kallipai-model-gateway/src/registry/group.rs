//! The `groups` entity: a user-run audience for collection publication.
//! Every user creates and manages their own groups; the everyone group is
//! a reserved row, not a special table.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "groups")]
pub struct Model {
    /// Opaque group id (minted by the handler, e.g. a ULID-style string).
    #[sea_orm(primary_key, auto_increment = false)]
    pub group_id: String,
    /// The creating account; groups are per-space like every registry row.
    pub owner: String,
    /// Owner-space-unique display name.
    pub name: String,
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
