//! The `provider_credentials` entity: the secret half of the provider
//! pool. Same composite key as `providers`, read only by the secret
//! module; never selected by distribution paths.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "provider_credentials")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub provider_id: String,
    /// The provider API key, plaintext (the forwarding path reads it once
    /// per request through the secret module's egress point).
    pub api_key: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
