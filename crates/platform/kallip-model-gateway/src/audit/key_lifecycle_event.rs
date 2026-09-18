//! The `key_lifecycle_events` entity: the audit trail of proxy-key
//! lifecycle transitions. The sole writer is the key admin face's
//! fail-closed mint/revoke transaction (crate::audit::record_lifecycle_event);
//! the forwarding path never writes here -- expiry is a derived state
//! read at resolution time. The event vocabulary is a CHECK-enforced
//! closed set (m_20260918_04).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "key_lifecycle_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub key_hash: Vec<u8>,
    pub tagma_id: String,
    /// Vocabulary: "issued" | "revoked" | "expired".
    pub event: String,
    pub detail: Option<String>,
    pub created_at: time::OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
