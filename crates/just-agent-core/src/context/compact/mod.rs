//! Compaction for context management.
//!
//! When the composed context exceeds the token budget, [`SummarizeStrategy`]
//! summarizes old turns via an LLM call and replaces them with a summary.

mod summarize;

use anyhow::Result;
use async_trait::async_trait;
use just_llm_client::ChatClient;

use super::turn::Turn;

pub use summarize::SummarizeStrategy;

/// Result of a compaction operation.
#[derive(Clone, Debug)]
pub struct CompactionResult {
    /// Summary text that replaces compacted turns.
    pub summary: String,
    /// Estimated tokens in the summary.
    pub summary_tokens: usize,
    /// Number of turns that were compacted.
    pub turns_compacted: usize,
}

/// Compaction strategy trait.
#[async_trait]
pub trait CompactionStrategy: Send + Sync {
    /// Human-readable name for diagnostics.
    fn name(&self) -> &str;

    /// Compact the given turns into a summary.
    async fn compact(
        &self,
        turns: &[Turn],
        existing_summary: Option<&str>,
        available: usize,
        client: &ChatClient,
    ) -> Result<CompactionResult>;
}
