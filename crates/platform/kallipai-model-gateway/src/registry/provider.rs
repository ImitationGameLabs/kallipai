//! The `providers` entity: a space-owned upstream endpoint identity.
//!
//! The non-secret half of the pool (family, base URL). The key lives in
//! the secret domain's `provider_credentials` row under the same composite
//! key and is never joined into this type. A profile references its
//! provider with the composite FK `(provider_id, owner)`, so a provider is
//! only ever shared inside its own space.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "providers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner: String,
    /// Owner-space-unique name. The creation path mints a provider with
    /// the creating profile's id; folded data names the group by its
    /// lowest member's id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub provider_id: String,
    /// Wire family of the upstream, in the client stack's spelling (the
    /// dispatch set `crate::forward::dialect::WIRE_FAMILIES` admits).
    /// Pool-level property: N profiles share one provider, so the
    /// family is the endpoint's property, not the deployment's.
    pub family: String,
    /// The upstream URL prefix before the wire path; NULL until a
    /// credential names one (a provider can exist unconfigured).
    pub base_url: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
