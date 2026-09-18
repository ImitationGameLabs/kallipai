//! The `tagmas` entity: the identity row behind every proxy key, carrying
//! the per-tagma side of the authorization matrix (the quota share).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "tagmas")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tagma_id: String,
    /// Cloud-account placeholder (design doc decision 5): always NULL until
    /// an account system exists -- the matrix key is single-dimensional
    /// (tagma) this phase.
    pub account_id: Option<String>,
    /// The per-tagma quota share. Budgets are BIGINT micro-units of the
    /// account currency; `None` = unlimited (the development default).
    pub max_budget: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
