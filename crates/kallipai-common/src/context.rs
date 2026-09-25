//! Context usage snapshot type.

/// Cumulative token usage across all LLM calls for an agent,
/// accumulated from exact provider-reported `Usage` values.
///
/// `cache_read_tokens` carries the `#[serde(alias)]` of the pre-migration single
/// cache counter, so legacy archives read back with the mandated mapping;
/// `cache_write_tokens` defaults to 0 for the same reason. Missing both cache
/// keys still fails: a partially-present object signals corruption and should
/// error rather than silently fill zeros. The parent's field-level default
/// already handles the case where this key is entirely absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CumulativeUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(alias = "cache_hit_tokens")]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl CumulativeUsage {
    /// Total tokens consumed (prompt + completion).
    ///
    /// Uses exact provider-reported values from `Usage`, not heuristic
    /// estimates. Semantically equivalent to `Usage.total_tokens` for
    /// every major provider.
    pub fn consumed(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Snapshot of current context layer breakdown and token usage.
///
/// `last_prompt_tokens` and `cumulative_usage` come from the provider's
/// response `usage` field — the most accurate token counts available.
/// Layer breakdowns use heuristic estimates for informational purposes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ContextUsage {
    /// Per-pinned-turn breakdown: (label, estimated_tokens).
    pub pinned_items: Vec<(String, usize)>,
    /// Number of stored conversation (non-pinned) turns.
    pub turn_count: usize,
    /// Estimated tokens across all conversation (non-pinned) turns.
    pub turn_tokens: usize,
    /// Exact prompt token count from the last provider response, if any.
    pub last_prompt_tokens: Option<u32>,
    /// Cumulative token usage across all LLM calls for this agent.
    #[serde(default)]
    pub cumulative_usage: CumulativeUsage,
    /// Largest conversation turns: up to five `(turn_id, estimated_tokens)`
    /// pairs, largest first. This is how an agent sees a queue-head wedge
    /// (one turn whose estimate alone exceeds the summarizer input budget)
    /// through `context_status` before compaction stalls on it.
    #[serde(default)]
    pub largest_turns: Vec<(u64, usize)>,
}

impl ContextUsage {
    pub fn format_summary(&self) -> String {
        let pinned_tokens: usize = self.pinned_items.iter().map(|(_, t)| *t).sum();
        format!(
            "turns: {} ({} est tokens), pinned: {} ({} tokens), last prompt: {}, cumulative: {} in / {} out / {} cache read / {} cache write",
            self.turn_count,
            crate::timefmt::humanize_count(self.turn_tokens as u64),
            self.pinned_items.len(),
            crate::timefmt::humanize_count(pinned_tokens as u64),
            self.last_prompt_tokens
                .map(|t| crate::timefmt::humanize_count(t as u64))
                .unwrap_or_else(|| "n/a".into()),
            crate::timefmt::humanize_count(self.cumulative_usage.prompt_tokens),
            crate::timefmt::humanize_count(self.cumulative_usage.completion_tokens),
            crate::timefmt::humanize_count(self.cumulative_usage.cache_read_tokens),
            crate::timefmt::humanize_count(self.cumulative_usage.cache_write_tokens),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::CumulativeUsage;

    #[test]
    fn legacy_single_cache_counter_maps_to_read_and_zero_write() {
        let usage: CumulativeUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":50,"cache_hit_tokens":10}"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.cache_read_tokens, 10);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn dual_cache_fields_round_trip() {
        let usage = CumulativeUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 4,
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("cache_read_tokens"));
        assert!(json.contains("cache_write_tokens"));
        assert!(
            !json.contains("cache_hit_tokens"),
            "writes use new names only"
        );
        let back: CumulativeUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn missing_both_cache_keys_is_corruption() {
        let parsed =
            serde_json::from_str::<CumulativeUsage>(r#"{"prompt_tokens":1,"completion_tokens":2}"#);
        assert!(
            parsed.is_err(),
            "partial object must fail rather than zero-fill"
        );
    }
}
