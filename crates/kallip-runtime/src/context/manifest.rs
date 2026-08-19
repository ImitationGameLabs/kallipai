//! On-disk projection types for split context persistence.

//! [`ContextStore`] itself is never serialized whole anymore. Instead it
//! projects onto two small documents: [`ManifestDoc`] (conversation-window
//! references plus the state that cannot be rebuilt from history) and
//! [`PinsDoc`] (the pinned layer in full, which history cannot reconstruct —
//! manual pins live only inside tool-call arguments and compaction summaries
//! land as un-IDed system records). Conversation turns are hydrated from the
//! append-only history log by turn ID; see `persistence::restore_agent`.

use just_llm_client::types::chat::ChatMessage;
use kallip_common::context::CumulativeUsage;
use kallip_common::retry::RetryRecord;
use serde::{Deserialize, Serialize};

/// Format version for both documents. Bump on any breaking shape change.
pub(crate) const FORMAT_VERSION: u64 = 1;

/// Reference-plus-state document written as `manifest.json` on every persist.
///
/// `conversation_turn_ids` is an explicit ascending list (not a first/last
/// range): an explicit list is self-checking — missing or gapped IDs are
/// detectable — and does not depend on eviction being a contiguous prefix
/// drain, which is an implementation detail rather than a format contract.
/// `last_prompt_tokens` is deliberately absent: restore already forces a full
/// re-estimate (`mark_needs_full_estimate`), never trusting a persisted anchor
/// across agent-version upgrades.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ManifestDoc {
    pub version: u64,
    /// IDs of the conversation (non-pinned, non-injected) turns that make up
    /// the live window, hydrated from history by ID.
    pub conversation_turn_ids: Vec<u64>,
    /// Exact provider-reported usage; history only carries estimates.
    pub cumulative_usage: CumulativeUsage,
    /// Next ID to assign. Derivable from history's max turn ID, but carried
    /// so the normal path never depends on that scan.
    pub next_turn_id: u64,
    /// Persistent retry history (not written to history).
    pub retry_log: Vec<RetryRecord>,
}

/// One pinned turn, in full. `message` is single-message by construction
/// (`pin`/`replace_pin` both store exactly one).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PinRecord {
    /// The pinned turn's stable ID. In-place `replace_pin` keeps it.
    pub id: u64,
    pub label: String,
    pub message: ChatMessage,
    pub estimated_tokens: usize,
}

/// Pinned-layer document written as `pins.json` whenever pins change.
/// Order is composition order: restore rebuilds the store's pinned prefix
/// from this list as-is.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PinsDoc {
    pub version: u64,
    pub pins: Vec<PinRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_doc_round_trips() {
        let doc = ManifestDoc {
            version: FORMAT_VERSION,
            conversation_turn_ids: vec![3, 7, 9],
            cumulative_usage: CumulativeUsage {
                prompt_tokens: 11,
                completion_tokens: 4,
                cache_hit_tokens: 2,
            },
            next_turn_id: 10,
            retry_log: vec![RetryRecord {
                timestamp: 1000,
                round: 1,
                attempt: 2,
                max_attempts: 10,
                error: "boom".into(),
                delay_secs: 1.5,
                endpoint: Some("deepseek".into()),
            }],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: ManifestDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.conversation_turn_ids, doc.conversation_turn_ids);
        assert_eq!(back.cumulative_usage.consumed(), 15);
        assert_eq!(back.next_turn_id, 10);
        assert_eq!(back.retry_log.len(), 1);
        assert_eq!(back.retry_log[0].endpoint.as_deref(), Some("deepseek"));
    }

    #[test]
    fn pins_doc_round_trips() {
        let doc = PinsDoc {
            version: FORMAT_VERSION,
            pins: vec![PinRecord {
                id: 42,
                label: "context_summary".into(),
                message: ChatMessage::assistant("summary text"),
                estimated_tokens: 12,
            }],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: PinsDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pins.len(), 1);
        assert_eq!(back.pins[0].id, 42);
        assert_eq!(back.pins[0].label, "context_summary");
        assert_eq!(back.pins[0].message.content(), Some("summary text"));
        assert_eq!(back.pins[0].estimated_tokens, 12);
    }
}
