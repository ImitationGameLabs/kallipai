//! SeaORM entity for the `task_events` trail.
//!
//! Every state move (kind=transition) and every action that never moves the
//! machine (kind=action: create, note, confirm, and the auditable --force
//! gate escapes) lands here with actor and timestamp.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "task_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub task_id: i64,
    /// "transition" | "action"
    pub kind: String,
    /// transition: start|pause|resume|review|close|reopen; action:
    /// create|note|confirm|force_start|force_resume|force_reopen|
    /// force_close|archive.
    pub name: String,
    /// Who triggered the event (the CLI caller).
    pub actor: Option<String>,
    /// Who executes the work at event time (dual position with actor).
    pub assignee: Option<String>,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    /// JSON payload (close reason/summary/archive hash, gate escapes, notes).
    pub payload: Option<String>,
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
