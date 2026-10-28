//! The `profile_sets` entity.

use sea_orm::entity::prelude::*;

/// A named, ordered profile set. Membership and order live in
/// `set_members`; this row is the set's identity + description.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profile_sets")]
pub struct Model {
    /// The owning space (part of the key: set names are unique per
    /// collection inside a space).
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    /// The set's home collection: part of the primary key, so a set name
    /// is unique inside its collection and may repeat in another. The
    /// composite foreign key keeps the anchor inside the set's own space.
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection_name: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    pub description: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
