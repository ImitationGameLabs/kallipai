//! The `profiles` entity.

use sea_orm::entity::prelude::*;

/// A distribution-face model profile. This model structurally carries no
/// secret material: the upstream endpoint and provider API key live in the
/// secret domain's `upstream_credentials` table and are never joined into
/// this type.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profiles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub profile_id: String,
    /// Wire family of the upstream (the design doc's openai-wire premise:
    /// `deepseek` / `openai-compatible`). The distribution face copies this
    /// verbatim; non-openai-wire upstreams are an admin-face registration
    /// concern (rejected there), not a distribution concern.
    pub family: String,
    pub model: String,
    pub max_context_window: Option<i64>,
    pub effort: Option<String>,
    /// JSON-encoded modality list (e.g. `["text"]`).
    pub modalities: Option<String>,
    /// Parking-resource flag: parked drafts are the `GET /parking` set.
    pub parked: bool,
    /// The total-quota dimension (the proxy-wide cap every tagma's
    /// consumption counts against). `None` = unlimited; budgets are BIGINT
    /// micro-units of the account currency.
    pub max_budget: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
