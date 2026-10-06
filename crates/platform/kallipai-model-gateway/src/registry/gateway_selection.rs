//! The `gateway_selections` entity: one tagma's selected collection
//! (the single store). The pointer names the owning space and the
//! collection in it; readers verify the target still exists and is
//! inside the viewer's domain before serving it.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "gateway_selections")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tagma_id: String,
    /// The space owning the selected collection (`system` = the
    /// platform catalog).
    pub owner: String,
    pub collection_name: String,
    pub updated_at: time::OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
