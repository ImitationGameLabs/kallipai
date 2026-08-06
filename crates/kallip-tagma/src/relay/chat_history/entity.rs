//! The sea-orm entity for the `chat_history` table, flattened out of an inline
//! nested module. Owned by the parent store module, which builds `ActiveModel`s
//! on append and maps `Model` rows to the decoded-read shape.

use sea_orm::entity::prelude::*;

/// One authored chat frame: outbound (agent -> user) or inbound (user ->
/// agent). `id` is a monotonic row id (AUTOINCREMENT: never reused, even
/// after GC), stamped onto the wire as `history_id` so the app can
/// dedup/order across batch replay and live delivery.
///
/// The sender is stored as typed columns -- `sender_kind` (CHECK-constrained
/// to `human`/`agent`, so the agent-free boundary "no agent id" is a
/// schema-level guarantee), `sender_id`, `sender_handle` -- and the content
/// as `text`, NOT as an opaque serialized-blob column. This makes every
/// future schema change an explicit migration (a `payload: Vec<u8>` would
/// hide wire-type changes as silent deserialize failures).
///
/// Invariant: a single tagma daemon owns exactly ONE conversation
/// (`conversation_id` is derived from the tagma id in `RelayHandle`),
/// so the column is constant within a DB. The schema is multi-conversation
/// shaped (column + `(conversation_id, id)` index) for forward
/// compatibility; if a future phase hosts multiple conversations per
/// tagma, only the GC cap needs scoping (see `gc` in the parent module).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "chat_history")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub conversation_id: String,
    /// `outbound` (agent -> user) or `inbound` (user -> agent). The replay
    /// loop reads `direction` to decide which wire reply shape to emit.
    #[sea_orm(column_type = "Text")]
    pub direction: String,
    /// The wire discriminant (`event`, `send_message`) for debugging /
    /// future filtering.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// Sender variant: `human` or `agent` (CHECK-constrained at the schema
    /// level). Never carries a tagma-internal agent id.
    #[sea_orm(column_type = "Text")]
    pub sender_kind: String,
    /// The sender's room-layer `ParticipantId` (the opaque derived
    /// identity -- `ParticipantId::for_user(...)` for a human,
    /// `for_tagma(...)` for the agent). May be empty on a backfilled
    /// outbound row from a never-enrolled tagma; `decode_row` refines it
    /// from the running tagma's id at read time.
    #[sea_orm(column_type = "Text")]
    pub sender_id: String,
    /// Display name. Advisory, sanitized at ingest; surfaced to the UI.
    #[sea_orm(column_type = "Text")]
    pub sender_handle: String,
    /// The message content (the assistant `content` for outbound, the
    /// user `text` for inbound). The current external vocabulary is a
    /// single string per frame; a future structured-content variant
    /// becomes another typed column via a new migration.
    #[sea_orm(column_type = "Text")]
    pub text: String,
    /// Unix seconds. Indexed indirectly via the id ordering; GC keys off
    /// this. i64 (not OffsetDateTime) to avoid time-format drift in
    /// SQLite and keep GC a plain integer compare.
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
