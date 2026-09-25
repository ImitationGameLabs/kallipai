//! The `request_audits` entity: one row per forwarded request. The request
//! log (status, latency, profile, key) and the per-key usage (token counts)
//! are the same row, so the audit views reconcile by construction.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "request_audits")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub key_hash: Vec<u8>,
    pub tagma_id: String,
    pub profile_id: String,
    /// The profile's deployment model (the request body is never parsed, so
    /// the client's requested model name is not available here).
    pub model: String,
    pub status_code: i32,
    pub duration_ms: i64,
    /// NULL when the upstream response carried no usage block.
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    /// Micro-units of the account currency; NULL until the pricing face
    /// lands (the max_budget check sums this column).
    pub cost_micros: Option<i64>,
    pub created_at: time::OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
