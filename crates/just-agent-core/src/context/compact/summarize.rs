use anyhow::Result;
use async_trait::async_trait;
use just_llm_client::ChatClient;
use just_llm_client::types::chat::ChatMessage;
use tracing::warn;

use super::{CompactionResult, CompactionStrategy};

const COMPACT_PROMPT: &str = "Summarize the key facts from our conversation so far: \
    user goals, decisions made, important outcomes, and the current state of work. \
    Be concise.";

/// LLM-powered summarization of old turns.
///
/// Incorporates any existing summary so summaries accumulate across
/// multiple compaction rounds rather than being replaced wholesale.
pub struct SummarizeStrategy {
    /// Maximum tokens for the generated summary.
    pub max_summary_tokens: u32,
    /// Prompt to use when requesting summarization.
    pub prompt: String,
}

impl SummarizeStrategy {
    pub fn new(max_summary_tokens: u32) -> Self {
        Self { max_summary_tokens, prompt: COMPACT_PROMPT.to_owned() }
    }
}

#[async_trait]
impl CompactionStrategy for SummarizeStrategy {
    fn name(&self) -> &str {
        "summarize"
    }

    async fn compact(
        &self,
        turns: &[super::super::turn::Turn],
        existing_summary: Option<&str>,
        available: usize,
        client: &ChatClient,
    ) -> Result<CompactionResult> {
        let mut summary_messages: Vec<ChatMessage> = Vec::new();
        let mut input_budget = available.saturating_sub(self.max_summary_tokens as usize);

        if let Some(existing) = existing_summary {
            let msg = ChatMessage::assistant(format!("[Previous context summary]\n{existing}"));
            input_budget =
                input_budget.saturating_sub(super::super::turn::estimate_message_tokens(&msg));
            summary_messages.push(msg);
        }

        // Fill from oldest turns forward, stopping when the input budget is exhausted.
        let mut turns_used = 0;
        for turn in turns.iter() {
            if turn.estimated_tokens > input_budget {
                break;
            }
            input_budget -= turn.estimated_tokens;
            turns_used += 1;
        }
        for turn in turns.iter().take(turns_used) {
            summary_messages.extend(turn.messages.iter().cloned());
        }

        summary_messages.push(ChatMessage::user(&self.prompt));

        let request = client
            .request(summary_messages)
            .with_max_tokens(self.max_summary_tokens);

        let response = client.create_chat_completion(request).await?;

        let (summary, summary_tokens) = match response
            .first_choice_content()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                let tokens = s.chars().count() / 4 + 16;
                (Some(s.to_owned()), tokens)
            }
            None => {
                warn!("compaction: LLM returned empty summary");
                (None, 0)
            }
        };

        Ok(CompactionResult {
            summary,
            summary_tokens,
            turns_compacted: turns_used,
            modified_turns: None,
        })
    }
}
