//! Decode a stored [`HistoryRow`] into the wire [`HistoryEntry`] shape, shared
//! by the relay history replay and the direct `/external/history` endpoint so
//! the two paths cannot drift on row interpretation.
//!
//! The store no longer persists any sender identity (the agent is reconstructed
//! at read; the peer is the `user_id`/`username` columns). So the caller
//! resolves the [`Participant`] sender per row — outbound ⇒ the agent
//! (`agent_sender`), inbound ⇒ the peer (`Participant` from `user_id`/`username`,
//! or the operator for `NULL`) — and passes it here. This module only maps
//! `direction` + `text` onto the wire reply shape.

use kallip_lesche_common::message::{HistoryEntry, Participant};

/// One row returned for re-encryption + emit by the history pull paths.
/// `direction` tells the replay loop which wire reply shape to reconstruct from
/// `text`; `user_id`/`username` are the peer identity (`None` = the operator on
/// the direct path) the caller resolves into the wire sender.
///
/// Constructed by the parent store's `history_row_from_model` and consumed here
/// by [`decode_row`]. The fields are `pub(crate)` (not just `pub(super)`)
/// because cross-module callers in `external.rs` read the raw fields directly
/// off the rows returned by the store's read paths.
pub(crate) struct HistoryRow {
    pub(crate) id: i64,
    pub(crate) user_id: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) direction: String,
    pub(crate) text: String,
    /// Unix seconds the row was appended. Surfaced to the wire as the frame's
    /// `created_at` so replayed history shows its original send time.
    pub(crate) created_at: i64,
}

/// Decode one stored [`HistoryRow`] into a [`HistoryEntry`] (`sender` + the
/// content-only wire reply), using the caller-resolved `sender`. Outbound rows
/// decode to a `TagmaReply::Event` (stamped with the row id + `created_at`);
/// inbound rows decode to a `TagmaReply::UserMessage` echo. Returns `None` for
/// an unknown `direction` so the caller skips it in replay.
pub(crate) fn decode_row(row: HistoryRow, sender: Participant) -> Option<HistoryEntry> {
    use kallip_common::protocol::AuthoredEvent;
    use kallip_lesche_common::message::TagmaReply;
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
