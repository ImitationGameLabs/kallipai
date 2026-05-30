//! Context usage snapshot type.

/// Cumulative token usage across all LLM calls in a session,
/// accumulated from exact provider-reported `Usage` values.
///
/// No `#[serde(default)]`: a partially-present object (e.g. new field added
/// to this struct but absent in persisted data) signals corruption and should
/// fail rather than silently filling zeros. The parent's field-level default
/// already handles the case where this key is entirely absent.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CumulativeUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_hit_tokens: u64,
}

/// Snapshot of current context layer breakdown and token usage.
///
/// `last_prompt_tokens` and `cumulative_usage` come from the provider's
/// response `usage` field — the most accurate token counts available.
/// Layer breakdowns use heuristic estimates for informational purposes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ContextUsage {
    /// Per-item breakdown: (label, estimated_tokens).
    pub pinned_items: Vec<(String, usize)>,
    /// Number of stored conversation turns.
    pub turn_count: usize,
    /// Estimated tokens across all turns.
    pub turn_tokens: usize,
    /// Exact prompt token count from the last provider response, if any.
    pub last_prompt_tokens: Option<u32>,
    /// Cumulative token usage across all LLM calls in this session.
    #[serde(default)]
    pub cumulative_usage: CumulativeUsage,
}

impl ContextUsage {
    pub fn format_summary(&self) -> String {
        let pinned_tokens: usize = self.pinned_items.iter().map(|(_, t)| *t).sum();
        format!(
            "turns: {} ({} est tokens), pinned: {} ({} tokens), last prompt: {}, cumulative: {} in / {} out / {} cache",
            self.turn_count,
            self.turn_tokens,
            self.pinned_items.len(),
            pinned_tokens,
            self.last_prompt_tokens
                .map(|t| t.to_string())
                .unwrap_or_else(|| "n/a".into()),
            self.cumulative_usage.prompt_tokens,
            self.cumulative_usage.completion_tokens,
            self.cumulative_usage.cache_hit_tokens,
        )
    }
}
