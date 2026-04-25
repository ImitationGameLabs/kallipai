use anyhow::Result;
use async_trait::async_trait;
use just_llm_client::ChatClient;
use just_llm_client::types::chat::{ChatMessage, ToolCallsMessage};

use super::{CompactionResult, CompactionStrategy};

/// Truncates individual long messages within turns.
///
/// Preserves turn structure but clips oversized tool results or
/// assistant messages to a maximum token budget per message.
pub struct TruncateStrategy {
    /// Maximum estimated tokens per individual message.
    pub max_message_tokens: usize,
    /// Notice appended to truncated messages.
    pub truncation_notice: String,
}

impl TruncateStrategy {
    pub fn new(max_message_tokens: usize) -> Self {
        Self { max_message_tokens, truncation_notice: "\n[truncated]".to_owned() }
    }
}

#[async_trait]
impl CompactionStrategy for TruncateStrategy {
    fn name(&self) -> &str {
        "truncate"
    }

    async fn compact(
        &self,
        turns: &[super::super::turn::Turn],
        existing_summary: Option<&str>,
        _available: usize,
        _client: &ChatClient,
    ) -> Result<CompactionResult> {
        let (summary, summary_tokens) = match existing_summary {
            Some(s) => {
                let tokens = s.chars().count() / 4 + 16;
                (Some(s.to_owned()), tokens)
            }
            None => (None, 0),
        };

        let max_chars = self.max_message_tokens.saturating_sub(16) * 4;
        let mut modified = Vec::with_capacity(turns.len());

        for turn in turns {
            let truncated_messages: Vec<ChatMessage> = turn
                .messages
                .iter()
                .map(|msg| truncate_message(msg, max_chars, &self.truncation_notice))
                .collect();

            let estimated_tokens = super::super::turn::Turn::estimate_tokens(&truncated_messages);
            modified.push(super::super::turn::Turn {
                id: turn.id,
                messages: truncated_messages,
                estimated_tokens,
            });
        }

        Ok(CompactionResult {
            summary,
            summary_tokens,
            turns_compacted: turns.len(),
            modified_turns: Some(modified),
        })
    }
}

/// Truncate a single message's text content to `max_chars` characters.
/// Messages without text content (e.g., tool-call-only messages) pass through unchanged.
fn truncate_message(msg: &ChatMessage, max_chars: usize, notice: &str) -> ChatMessage {
    let content = match msg.content() {
        Some(c) => c,
        None => return msg.clone(),
    };

    if content.chars().count() <= max_chars {
        return msg.clone();
    }

    let truncated: String = content
        .chars()
        .take(max_chars)
        .chain(notice.chars())
        .collect();

    match msg {
        ChatMessage::ToolResult(tr) => ChatMessage::tool_result(truncated, &tr.tool_call_id),
        ChatMessage::ToolCalls(tc) => {
            ChatMessage::ToolCalls(ToolCallsMessage { content: Some(truncated), ..(*tc).clone() })
        }
        ChatMessage::Message(m) => ChatMessage::new(&m.role, truncated),
    }
}
