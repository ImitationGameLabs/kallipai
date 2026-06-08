//! Logical conversation unit for context management.
//!
//! A [`Turn`] groups the messages from one agent loop iteration,
//! keeping tool call/result pairs coherent across eviction.

use serde::{Deserialize, Serialize};

use just_llm_client::types::chat::ChatMessage;

/// Stable unique identifier for a turn within an agent's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnId(pub u64);

/// A logical conversation turn: one or more messages produced in a
/// single agent loop iteration.
///
/// A turn typically contains one of:
/// - A user message (initial prompt)
/// - An assistant tool-call message + corresponding tool-result messages
/// - A final assistant text response
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub messages: Vec<ChatMessage>,
    /// Cached token estimate computed on insertion.
    pub estimated_tokens: usize,
}

impl Turn {
    /// Estimate the token count for a slice of messages using a
    /// char-div-4 heuristic with per-message overhead.
    pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
        messages.iter().map(estimate_message_tokens).sum()
    }
}

/// Per-message token estimate: content chars / 4 + tool-call args / 4 +
/// per-call overhead + per-message overhead.
pub(crate) fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let content_tokens = message
        .content()
        .map(|c| c.chars().count() / 4)
        .unwrap_or_default();

    let tool_tokens = message
        .tool_calls()
        .map(|calls| {
            calls
                .iter()
                .map(|tc| tc.function.arguments.chars().count() / 4 + 24)
                .sum::<usize>()
        })
        .unwrap_or_default();

    content_tokens + tool_tokens + 16
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
}
