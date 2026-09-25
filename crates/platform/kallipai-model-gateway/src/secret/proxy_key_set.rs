//! The `proxy_key_sets` entity: a proxy key's allowed set names.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "proxy_key_sets")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key_hash: Vec<u8>,
    #[sea_orm(primary_key, auto_increment = false)]
    pub set_name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
