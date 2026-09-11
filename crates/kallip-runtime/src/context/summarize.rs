//! Context summarization: reduces old turns into a pinned summary.
//!
//! When the composed context exceeds the token budget,
//! [`ContextSummarizer`] summarizes old turns via an LLM call,
//! pins the summary, and the caller evicts the summarized turns.

use crate::profile::GenerationClient;
use anyhow::{Result, bail};
use just_llm_client::types::generation::{ContentPart, Message, MessageContent};

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
        client: &GenerationClient,
    ) -> Result<(Summary, Option<just_llm_client::types::generation::Usage>)> {
        let mut messages: Vec<Message> = Vec::new();
        let mut input_budget = available.saturating_sub(self.max_tokens as usize);

        if let Some(existing) = existing_summary {
            let msg = Message::assistant(format!("[Previous context summary]\n{existing}"));
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
            messages.extend(turn.messages.iter().map(summarizer_view));
        }

        messages.push(Message::user(&self.prompt));

        let request = client
            .create_request(messages)
            .with_max_tokens(self.max_tokens);

        let response = client.generate(request).await?;
        let usage = response.usage.clone();

        let text = match response.text().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => s.to_owned(),
            None => bail!("summarization: LLM returned empty summary"),
        };
        let estimated_tokens = super::tokens::estimate_message_tokens(&Message::assistant(&text));

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

/// The summarizer input view of a message: image parts swapped for the
/// [`super::IMAGE_PLACEHOLDER`] text. The summarizer consumes words, not
/// pixels, and a raw Base64 blob would flood both the request and the
/// input budget; the placeholder keeps the image's existence visible so
/// the summary can mention it.
fn summarizer_view(message: &Message) -> Message {
    let Some(parts) = message.content_parts() else {
        return message.clone();
    };
    if !parts.iter().any(|p| matches!(p, ContentPart::Image { .. })) {
        return message.clone();
    }
    let swapped = parts
        .iter()
        .map(|part| match part {
            ContentPart::Image { .. } => ContentPart::Text {
                text: super::IMAGE_PLACEHOLDER.to_owned(),
            },
            other => other.clone(),
        })
        .collect();
    match message {
        Message::User { .. } => Message::User {
            content: MessageContent::Parts(swapped),
        },
        _ => Message::System {
            content: MessageContent::Parts(swapped),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_message() -> Message {
        Message::user_parts(vec![
            ContentPart::Text {
                text: "chart caption".to_owned(),
            },
            ContentPart::Image {
                source: just_llm_client::types::generation::ImageSource::Base64 {
                    data: "aGk=".to_owned(),
                    media_type: "image/png".to_owned(),
                },
                detail: None,
            },
        ])
    }

    #[test]
    fn summarizer_view_swaps_images_for_the_placeholder() {
        let view = summarizer_view(&parts_message());
        let Message::User {
            content: MessageContent::Parts(parts),
        } = &view
        else {
            panic!("expected a user parts message");
        };
        assert!(!parts.iter().any(|p| matches!(p, ContentPart::Image { .. })));
        assert!(parts.iter().any(
            |p| matches!(p, ContentPart::Text { text } if text == crate::context::IMAGE_PLACEHOLDER)
        ));
    }

    #[test]
    fn summarizer_view_leaves_plain_messages_untouched() {
        let plain = Message::user("just words");
        assert_eq!(summarizer_view(&plain), plain);
    }
}
