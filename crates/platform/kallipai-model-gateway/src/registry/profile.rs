//! The `profiles` entity.

use sea_orm::entity::prelude::*;

/// A distribution-face model profile. This model structurally carries no
/// secret material: the upstream endpoint and provider API key live in the
/// secret domain's `provider_credentials` table and are never joined into
/// this type. The owner is part of the primary key, so ids are unique
/// within a space; a global unique index makes each id unique across
/// spaces.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profiles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    /// The ownership space: the platform catalog (`system`, the
    /// distribution face's only source) or the owning account's private
    /// space. Written by the parking create and the set-membership
    /// promote on the management face; read everywhere, served only in the
    /// catalog.
    pub owner: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub profile_id: String,
    /// The space-owned provider this deployment runs on (the composite
    /// FK `(provider_id, owner)` pins the reference inside this space).
    /// The wire family lives on the provider; N profiles share one.
    pub provider_id: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    /// JSON-encoded modality list (e.g. `["text"]`).
    pub modalities: Option<String>,
    /// Parking-resource flag: parked drafts are the `GET /parking` set.
    pub parked: bool,
    /// The responses wire's server-side transcript tri-state: `true`
    /// persists the response upstream, `false` does not, `NULL` = no
    /// explicit choice (the client's request body decides). Carried
    /// through the distribution face, never derived here.
    pub store: Option<bool>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
