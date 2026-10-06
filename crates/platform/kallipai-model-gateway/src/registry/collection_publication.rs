//! The `collection_publications` entity: one row per live publication of
//! a collection into a group. Withdrawal deletes the row; the
//! publication IS the row (no state column to drift).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "collection_publications")]
pub struct Model {
    /// The publishing collection's owner (the publisher).
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection_name: String,
    /// The audience the collection is published to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub group_id: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
