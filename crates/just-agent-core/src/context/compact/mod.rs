//! Pluggable compaction strategies for context management.
//!
//! When the composed context exceeds the token budget, a [`CompactionStrategy`]
//! is invoked to reduce the stored turns. Three strategies are provided:
//!
//! - [`EvictStrategy`] — drops turns outright
//! - [`SummarizeStrategy`] — uses the LLM to summarize old turns
//! - [`TruncateStrategy`] — truncates long messages within turns

mod evict;
mod summarize;
mod truncate;

use anyhow::Result;
use async_trait::async_trait;
use just_llm_client::ChatClient;

use super::turn::Turn;

pub use evict::EvictStrategy;
pub use summarize::SummarizeStrategy;
pub use truncate::TruncateStrategy;

/// Result of a compaction operation.
#[derive(Clone, Debug)]
pub struct CompactionResult {
    /// Summary text that replaces compacted turns (if any).
    pub summary: Option<String>,
    /// Estimated tokens in the summary.
    pub summary_tokens: usize,
    /// Number of turns that were compacted.
    pub turns_compacted: usize,
    /// Modified turns to re-insert instead of discarding (used by TruncateStrategy).
    pub modified_turns: Option<Vec<Turn>>,
}

/// Pluggable compaction strategy.
#[async_trait]
pub trait CompactionStrategy: Send + Sync {
    /// Human-readable name for diagnostics.
    fn name(&self) -> &str;

    /// Compact the given turns into a replacement.
    ///
    /// The caller provides the current summary (if any) so the strategy
    /// can incorporate it.
    async fn compact(
        &self,
        turns: &[Turn],
        existing_summary: Option<&str>,
        available: usize,
        client: &ChatClient,
    ) -> Result<CompactionResult>;
}

/// Creates the default compaction strategy from config values.
pub fn strategy_from_name(name: &str, max_summary_tokens: u32) -> Box<dyn CompactionStrategy> {
    match name {
        "evict" => Box::new(EvictStrategy),
        "truncate" => Box::new(TruncateStrategy::new(2_000)),
        _ => Box::new(SummarizeStrategy::new(max_summary_tokens)),
    }
}
