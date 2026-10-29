//! The `profiles` entity.

use sea_orm::entity::prelude::*;

/// A distribution-face model profile. This model structurally carries no
/// secret material: the upstream endpoint and provider API key live in the
/// secret domain's `provider_credentials` table and are never joined into
/// this type. Keys are collection-scoped: `(owner, collection_name,
/// profile_id)` is the primary key, so an id is unique inside its
/// collection and the same id may exist in another collection.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profiles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    /// The ownership space: the platform catalog (`system`, the
    /// distribution face's only source) or the owning account's private
    /// space. Written by the profile create, update, and move handlers on
    /// the management face; read everywhere, served only in the catalog.
    pub owner: String,
    /// The collection this row belongs to: part of the primary key. A
    /// profile belongs to exactly one collection; the composite key
    /// makes its id unique there and nowhere else.
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection_name: String,
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
    /// The set this profile is a member of, if any. `NULL` = unassigned
    /// (the distribution face does not serve it); a name = the profile's
    /// one and only set, pinned inside the same collection by the
    /// composite foreign key `(owner, collection_name, set_name)`.
    pub set_name: Option<String>,
    /// Failover order inside `set_name` (meaningless while unassigned):
    /// reads order by `(position, profile_id)`, deterministic but not
    /// enforced distinct.
    pub position: i32,
    /// The responses wire's server-side transcript tri-state: `true`
    /// persists the response upstream, `false` does not, `NULL` = no
    /// explicit choice (the client's request body decides). Carried
    /// through the distribution face, never derived here.
    pub store: Option<bool>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
