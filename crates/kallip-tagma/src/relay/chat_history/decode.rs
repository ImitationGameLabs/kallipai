//! Decode a stored [`HistoryRow`] into the wire [`HistoryEntry`] shape, shared
//! by the relay history replay and the direct `/external/history` endpoint so
//! the two paths cannot drift on row interpretation.

use kallip_lesche_common::message::{HistoryEntry, Participant};

/// One row returned for re-encryption + emit by the history pull paths.
/// `direction` tells the replay loop which wire reply shape to reconstruct from
/// `text`; the typed `sender_*` fields pair into the [`HistoryEntry`] sender.
///
/// Constructed by the parent store's `history_row_from_model` and consumed here
/// by [`decode_row`]. The fields are `pub(crate)` (not just `pub(super)`)
/// because cross-module callers in `external.rs` read the raw fields directly
/// off the rows returned by the store's read paths.
pub(crate) struct HistoryRow {
    pub(crate) id: i64,
    pub(crate) direction: String,
    pub(crate) sender_kind: String,
    pub(crate) sender_id: String,
    pub(crate) sender_handle: String,
    pub(crate) text: String,
    /// Unix seconds the row was appended. Surfaced to the wire as the frame's
    /// `created_at` so replayed history shows its original send time.
    pub(crate) created_at: i64,
}

/// Decode one stored [`HistoryRow`] into a [`HistoryEntry`] (sender + the
/// content-only wire reply). Outbound rows decode to a `TagmaReply::Event`
/// (re-stamped with the row id + `created_at`); inbound rows decode to a
/// `TagmaReply::UserMessage` echo. Returns `None` for rows that cannot be
/// interpreted (an unknown `sender_kind`, an unknown `direction`) so the caller
/// skips them in replay. The CHECK constraint guarantees `sender_kind ∈
/// {human, agent}`, so the only `None` paths are direction/shape corruption.
///
/// Note: a backfilled outbound row from a never-enrolled tagma carries an empty
/// `sender_id`; the projector's `read_history` refines it to the running
/// tagma's id.
pub(crate) fn decode_row(row: HistoryRow) -> Option<HistoryEntry> {
    use kallip_agora_common::ids::{ParticipantId, ParticipantKind};
    use kallip_common::protocol::AuthoredEvent;
    use kallip_lesche_common::message::TagmaReply;
    let sender = match row.sender_kind.as_str() {
        "human" => Participant {
            id: ParticipantId::from(row.sender_id),
            kind: ParticipantKind::Human,
            handle: row.sender_handle,
            tagma_id: None,
        },
        "agent" => Participant {
            id: ParticipantId::from(row.sender_id),
            kind: ParticipantKind::Agent,
            handle: row.sender_handle,
            tagma_id: None,
        },
        other => {
            tracing::warn!(
                id = row.id,
                sender_kind = other,
                "unknown sender_kind; skipping"
            );
            return None;
        }
    };
    let reply = match row.direction.as_str() {
        "outbound" => {
            let mut r = TagmaReply::Event {
                event: AuthoredEvent::AssistantContent { content: row.text },
                history_id: row.id,
                created_at: None,
            };
            r.set_created_at(row.created_at);
            r
        }
        "inbound" => {
            let mut r = TagmaReply::UserMessage {
                history_id: row.id,
                text: row.text,
                created_at: None,
            };
            r.set_created_at(row.created_at);
            r
        }
        other => {
            tracing::warn!(
                id = row.id,
                direction = other,
                "unknown direction; skipping"
            );
            return None;
        }
    };
    Some(HistoryEntry { sender, reply })
}
