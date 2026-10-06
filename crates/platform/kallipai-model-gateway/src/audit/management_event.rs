//! The `management_events` entity: the admin face's change audit log.
//!
//! Written inside the same transaction as the change it describes -- see
//! [`record`] for why a management change and its audit row commit or
//! roll back together. The entity is read-only for every other
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
    /// "profile_set" | "set_member" | "profile" |
    /// "provider_credential".
    pub entity: String,
    /// The row identity the action targeted (set name, profile id --
    /// natural keys, so the log reads without joins).
    pub entity_id: String,
    /// The operator that made the change (the admin face's attribution:
    /// the local administrator writes `admin`; account operators write
    /// the acting account's id).
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
    /// The actor behind the change (the handler's extractor): the admin
    /// constant or the signed-in user's id.
    pub actor: std::borrow::Cow<'static, str>,
    pub before: Option<Json>,
    pub after: Option<Json>,
}

/// Land one management change row inside the caller's transaction.
///
/// A management change and this row are the same fact: the caller
/// commits them together and any failure rolls both back. Keep this
/// path transactional; never weaken it to a best-effort write.
pub(crate) async fn record<C: ConnectionTrait>(
    txn: &C,
    row: ManagementEventRow,
) -> Result<(), DbErr> {
    ActiveModel {
        action: Set(row.action.to_owned()),
        entity: Set(row.entity.to_owned()),
        entity_id: Set(row.entity_id),
        actor: Set(row.actor.into_owned()),
        before: Set(row.before),
        after: Set(row.after),
        created_at: Set(time::OffsetDateTime::now_utc()),
        ..Default::default()
    }
    .insert(txn)
    .await
    .map(|_| ())
}
