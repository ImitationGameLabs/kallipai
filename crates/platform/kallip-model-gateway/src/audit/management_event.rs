//! The `management_events` entity: the admin face's change audit log.
//!
//! Written inside the same transaction as the change it describes -- see
//! [`record`] for why this write is fail-closed where the request audit is
//! deliberately best-effort. The entity is read-only for every other
//! module: handlers hand their before/after states to [`record`], nothing
//! edits or deletes a landed row.

use sea_orm::Set;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "management_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// "create" | "update" | "delete" (free text by design, see the
    /// migration's vocabulary comment).
    pub action: String,
    /// "profile_set" | "set_member" | "profile" | "registry_meta" |
    /// "upstream_credential" | "proxy_key".
    pub entity: String,
    /// The row identity the action targeted (set name, profile id, meta
    /// key -- natural keys, so the log reads without joins).
    pub entity_id: String,
    /// The credential that made the change ("management-token" for the
    /// static token; a future dynamic credential writes its principal).
    pub actor: String,
    /// The row state before the change; `None` on create. JSONB: the
    /// handler's serde view of the entity.
    pub before: Option<Json>,
    /// The row state after the change; `None` on delete.
    pub after: Option<Json>,
    pub created_at: time::OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The handler-side description of one landed change.
pub(crate) struct ManagementEventRow {
    pub action: &'static str,
    pub entity: &'static str,
    pub entity_id: String,
    pub before: Option<Json>,
    pub after: Option<Json>,
}

/// Land one management change row inside the caller's transaction.
///
/// This is the deliberate opposite of [`super::record_request`]: the
/// request audit is best-effort (warn-and-move-on) because the upstream
/// cost is already spent when it runs, while a management change has no
/// such hedge -- the change and this row are the same fact, so the caller
/// commits them together and any failure rolls both back. Never weaken
/// this to best-effort by analogy with the forwarding path.
pub(crate) async fn record<C: ConnectionTrait>(
    txn: &C,
    row: ManagementEventRow,
) -> Result<(), DbErr> {
    ActiveModel {
        action: Set(row.action.to_owned()),
        entity: Set(row.entity.to_owned()),
        entity_id: Set(row.entity_id),
        actor: Set(super::MANAGEMENT_ACTOR.to_owned()),
        before: Set(row.before),
        after: Set(row.after),
        created_at: Set(time::OffsetDateTime::now_utc()),
        ..Default::default()
    }
    .insert(txn)
    .await
    .map(|_| ())
}
