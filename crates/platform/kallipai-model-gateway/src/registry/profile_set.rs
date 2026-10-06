//! The `profile_sets` entity.

use sea_orm::entity::prelude::*;

/// A named, ordered profile set. Membership and order live in
/// `set_members`; this row is the set's identity + description.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profile_sets")]
pub struct Model {
    /// The owning space (part of the key: set names are unique per
    /// space, not globally).
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    pub description: String,
    /// The set's home collection: the membership authority. A set is
    /// born anchored (the creation doors name the collection) and
    /// stays anchored for life; the composite foreign key keeps the
    /// anchor inside the set's own space.
    pub collection_name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
