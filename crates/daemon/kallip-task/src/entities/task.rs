//! SeaORM entity for the `tasks` flat table.
//!
//! One row per task: the four timestamps (created/updated/started/ended),
//! the current coarse state, the confirmation registration (creator, assignee,
//! registered confirmers), the two-level terminal
//! state (`closed` + reason), the association keys (an inbox id range
//! and/or a lesche room + seq range in the K8s involvedObject shape),
//! and the closed-archive pointer (content address + entry count).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tasks")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub title: String,
    pub status: String,
    pub creator: Option<String>,
    pub assignee: Option<String>,
    /// JSON array of registered confirmer identities (fixed at create).
    pub confirmers: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Set at first start; preserved across reopen (the original start
    /// line stays the task's start line).
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    /// Query partition marker, not a state: archived tasks leave the
    /// default list view (`task list --archived` shows them). Set by
    /// `task archive`; the gate requires `closed` (the escape is recorded).
    pub archived: i64,
    pub archived_at: Option<i64>,
    pub closed_reason: Option<String>,
    /// One-sentence result of the task, recorded at close (close-gate product).
    pub close_summary: Option<String>,
    /// Association key: inbox id window circumscribing the task's trail.
    pub inbox_id_start: Option<i64>,
    pub inbox_id_end: Option<i64>,
    pub room_id: Option<String>,
    pub room_seq_start: Option<i64>,
    pub room_seq_end: Option<i64>,
    /// The closed archive: the content address, plus the tar entry
    /// count frozen at close. Both absent until a close with a dossier.
    pub archive_hash: Option<String>,
    pub archive_entries: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
