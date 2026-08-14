//! SeaORM entity for the `inbox_events` table.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "inbox_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub agent_id: String,
    pub timestamp: i64,
    pub source: String,
    pub body: String,
    pub status: String,
    pub delivered: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
