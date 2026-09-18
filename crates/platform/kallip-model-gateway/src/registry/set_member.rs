//! The `set_members` entity: ordered membership (position is failover
//! semantics, returned verbatim by every read path).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "set_members")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub set_name: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub profile_id: String,
    pub position: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
