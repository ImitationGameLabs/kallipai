//! The `platform_groups` entity: the platform side of the reach layer
//! (the platform/admin split). Groups the admin face manages; split from
//! `groups` table so the two domains publish through disjoint sides of
//! the wall.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "platform_groups")]
pub struct Model {
    /// Opaque group id (minted by the handler; `everyone` is the
    /// reserved sentinel).
    #[sea_orm(primary_key, auto_increment = false)]
    pub group_id: String,
    /// The managing account; the admin face writes under the catalog
    /// owner.
    pub owner: String,
    /// Owner-space-unique display name.
    pub name: String,
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
