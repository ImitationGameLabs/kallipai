use anyhow::Result;
use async_trait::async_trait;
use just_llm_client::ChatClient;

use super::{CompactionResult, CompactionStrategy};

/// Drops all provided turns outright. No LLM call needed.
///
/// If an existing summary is present it is preserved. Use this strategy
/// when you want to free context aggressively without spending tokens
/// on summarization.
pub struct EvictStrategy;

#[async_trait]
impl CompactionStrategy for EvictStrategy {
    fn name(&self) -> &str {
        "evict"
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
        Ok(CompactionResult {
            summary,
            summary_tokens,
            turns_compacted: turns.len(),
            modified_turns: None,
        })
    }
}
