//! The `set_members` entity: ordered membership (position is failover
//! semantics, returned verbatim by every read path).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "set_members")]
pub struct Model {
    /// The owning set's space (a member row's owner is its set's owner:
    /// membership never crosses a space boundary).
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    /// The owning set's collection: membership never crosses a
    /// collection either (the composite foreign keys pin both the set
    /// and the profile inside it).
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection_name: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub set_name: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub profile_id: String,
    pub position: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
