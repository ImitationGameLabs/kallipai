//! The `group_members` entity: an account's membership in a group. The
//! member is an archeion account id (TEXT, no cross-service FK -- the
//! `owner` column's precedent).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "group_members")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub group_id: String,
    /// The member's archeion `users.id`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub member_account: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
