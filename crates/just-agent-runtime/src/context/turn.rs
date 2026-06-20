//! Entries in the agent's context log.
//!
//! A [`Turn`] is either a conversation turn (messages from one agent loop iteration,
//! evictable) or a pinned entry (persistent labeled context — a compaction summary, skill, or
//! note — never evicted). The store keeps all pinned turns before conversation turns.

use serde::{Deserialize, Serialize};

use just_llm_client::types::chat::ChatMessage;

/// Stable unique identifier for a turn within an agent's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnId(pub u64);

/// Kind of a [`Turn`]: a normal conversation turn, or pinned persistent context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TurnKind {
    /// A normal conversation turn (evictable, chronological).
    Conversation,
    /// Pinned persistent context — a compaction summary, loaded skill, or agent note.
    /// Never evicted; always composed before conversation turns; identified by `label` for
    /// replace/remove-by-label (e.g. `"context_summary"`, `"skill:foo"`).
    Pinned { label: String },
}

impl Default for TurnKind {
    /// Defaults to `TurnKind::Conversation` so old serialized turns (which predate the
    /// `kind` field) deserialize as ordinary conversation turns.
    fn default() -> Self {
        Self::Conversation
    }
}

/// One entry in the agent's context log.
///
/// A `Conversation` turn holds the messages produced in a single agent loop iteration —
/// typically a user message, an assistant tool-call message plus its tool-result messages, or
/// a final assistant response.
///
/// A `Pinned` turn holds persistent labeled context (a compaction summary, loaded skill, or
/// agent note) that is never evicted and is always composed before conversation turns. See
/// [`TurnKind`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub messages: Vec<ChatMessage>,
    /// Cached token estimate computed on insertion.
    pub estimated_tokens: usize,
    /// Whether this is pinned persistent context or a normal conversation turn.
    /// `#[serde(default)]` so old turns deserialize as `TurnKind::Conversation`.
    #[serde(default)]
    pub kind: TurnKind,
}

impl Turn {
    /// Whether this is a pinned persistent-context turn.
    pub fn is_pinned(&self) -> bool {
        matches!(self.kind, TurnKind::Pinned { .. })
    }

    /// The label identifying a pinned turn, or `None` for conversation turns.
    pub fn label(&self) -> Option<&str> {
        match &self.kind {
            TurnKind::Pinned { label } => Some(label),
            TurnKind::Conversation => None,
        }
    }

    /// Estimate the token count for a slice of messages via the crate's token-estimation seam
    /// (`context::tokens`).
    pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(super::tokens::estimate_message_tokens)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_scales_with_content() {
        let short = Turn::estimate_tokens(&[ChatMessage::user("hello")]);
        let long = Turn::estimate_tokens(&[ChatMessage::user("x".repeat(400))]);
        assert!(long > short);
    }

    #[test]
    fn estimate_tokens_accounts_for_tool_calls() {
        use just_llm_client::types::chat::{ChatToolCall, FunctionCall, ToolType};

        let plain = Turn::estimate_tokens(&[ChatMessage::assistant("hello")]);
        let with_tools =
            Turn::estimate_tokens(&[ChatMessage::assistant_tool_calls(vec![ChatToolCall {
                id: "call_1".into(),
                kind: ToolType::Function,
                function: FunctionCall {
                    name: "test".into(),
                    arguments: r#"{"key": "value"}"#.into(),
                },
            }])]);
        assert!(with_tools > plain);
    }

    #[test]
    fn turnkind_default_is_conversation() {
        assert!(matches!(TurnKind::default(), TurnKind::Conversation));
    }

    #[test]
    fn turn_without_kind_deserializes_as_conversation() {
        // A turn serialized without the `kind` field (legacy format) deserializes as Conversation.
        let json = r#"{"id":0,"messages":[],"estimated_tokens":0}"#;
        let turn: Turn = serde_json::from_str(json).unwrap();
        assert!(!turn.is_pinned());
        assert_eq!(turn.label(), None);
    }
}
