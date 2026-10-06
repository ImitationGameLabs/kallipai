//! The `platform_publications` entity: the platform side of the reach
//! layer (the platform/admin split). One row per live publication of a
//! platform group; the publication IS the row (no state column to
//! drift). The user-side `collection_publications` table keeps serving
//! the user domain; the visibility face reads the shared shape.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "platform_publications")]
pub struct Model {
    /// The publishing collection's owner (the catalog owner on the
    /// platform side).
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection_name: String,
    /// The audience the collection is published to (a platform group).
    #[sea_orm(primary_key, auto_increment = false)]
    pub group_id: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
