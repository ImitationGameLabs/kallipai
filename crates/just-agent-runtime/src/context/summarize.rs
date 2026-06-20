//! Context summarization: reduces old turns into a pinned summary.
//!
//! When the composed context exceeds the token budget,
//! [`ContextSummarizer`] summarizes old turns via an LLM call,
//! pins the summary, and the caller evicts the summarized turns.

use crate::profile::ChatClient;
use anyhow::{Result, bail};
use just_llm_client::types::chat::ChatMessage;

use super::turn::Turn;

const SUMMARIZE_PROMPT: &str = "Summarize the key facts from our conversation so far: \
    user goals, decisions made, important outcomes, and the current state of work. \
    Be concise.";

/// Summary produced by [`ContextSummarizer`].
#[derive(Clone, Debug)]
pub struct Summary {
    /// Summary text, pinned as a `context_summary` pinned item.
    pub text: String,
    /// Token estimate via `context::tokens`. For diagnostic logging only;
    /// the pinned turn's tokens are tracked via `estimate_message_tokens`.
    pub estimated_tokens: usize,
    /// Number of source turns this summary covers.
    pub source_turns: usize,
}

/// LLM-powered summarization of old conversation turns.
///
/// Incorporates any existing summary so summaries accumulate across
/// multiple rounds rather than being replaced wholesale.
pub struct ContextSummarizer {
    /// Maximum tokens for the generated summary.
    pub max_tokens: u32,
    /// Prompt to use when requesting summarization.
    pub prompt: String,
}

impl ContextSummarizer {
    pub fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            prompt: SUMMARIZE_PROMPT.to_owned(),
        }
    }

    /// Summarize the given turns via an LLM call.
    ///
    /// Returns `(Summary, Option<Usage>)` — the summary text and the exact
    /// token usage from the provider response (for budget tracking).
    pub async fn summarize(
        &self,
        turns: &[Turn],
        existing_summary: Option<&str>,
        available: usize,
        client: &ChatClient,
    ) -> Result<(Summary, Option<just_llm_client::types::chat::Usage>)> {
        let mut messages: Vec<ChatMessage> = Vec::new();
        let mut input_budget = available.saturating_sub(self.max_tokens as usize);

        if let Some(existing) = existing_summary {
            let msg = ChatMessage::assistant(format!("[Previous context summary]\n{existing}"));
            input_budget =
                input_budget.saturating_sub(super::tokens::estimate_message_tokens(&msg));
            messages.push(msg);
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
            messages.extend(turn.messages.iter().cloned());
        }

        messages.push(ChatMessage::user(&self.prompt));

        let request = client
            .create_request(messages)
            .with_max_tokens(self.max_tokens);

        let response = client.chat_completion(request).await?;
        let usage = response.usage.clone();

        let text = match response
            .first_choice_content()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.to_owned(),
            None => bail!("summarization: LLM returned empty summary"),
        };
        let estimated_tokens =
            super::tokens::estimate_message_tokens(&ChatMessage::assistant(&text));

        Ok((
            Summary {
                text,
                estimated_tokens,
                source_turns: turns_used,
            },
            usage,
        ))
    }
}
