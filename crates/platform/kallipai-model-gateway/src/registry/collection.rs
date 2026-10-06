//! The `collections` entity: the publishable bundle above sets. Ordered
//! only by name; the sets inside a collection are unordered (each set
//! keeps its own failover order). The user face populates the table.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "collections")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    pub description: String,
    /// The collection's default-set anchor: the first set landing in
    /// a default-less collection takes the slot conditionally, so
    /// concurrent creators never race the anchor (`NULL` = none yet).
    pub default_set_name: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
