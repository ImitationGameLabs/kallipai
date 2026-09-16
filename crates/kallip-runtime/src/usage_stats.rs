//! Per-agent, single-launch token usage counters.
//!
//! In-memory only, process-lifetime scoped: the map starts empty at tagma
//! boot and fills as agents record LLM usage. This is the per-agent split of
//! the same "since process start" semantics [`crate::token_budget::TokenBudget`]
//! carries tagma-wide; the lifetime, on-disk cumulative totals in
//! `ContextStore` are a separate view and the two never mix.
//!
//! Clones share one `Arc`, so `AppState` and every `AgentContext` observe the
//! same counters. The lock is a plain `std::sync::Mutex` and is never held
//! across an await point — every method does arithmetic under the lock and
//! returns owned data, following the `TokenBudget` precedent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use just_llm_client::types::generation::Usage;
use kallip_common::AgentId;

/// One agent's single-launch token totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageEntry {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
}

impl UsageEntry {
    /// Cache-hit ratio: cache reads over prompt tokens. A zero prompt total
    /// yields 0.0 — no recorded calls carry no meaningful hit rate.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / self.prompt_tokens as f64
        }
    }
}

/// Sums over every recorded agent (single-launch scope).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
}

impl UsageTotals {
    /// Cache-hit ratio over summed counters: total cache reads divided by
    /// total prompt tokens — sum first, then divide. Never an average of
    /// per-agent ratios, which would weight a one-call agent equally with a
    /// thousand-call one. Zero total prompts yield 0.0.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / self.prompt_tokens as f64
        }
    }
}

/// Per-agent single-launch token usage, shared process-wide.
///
/// Cloned from `AppState` into every `AgentContext` (same underlying map) so
/// the two accounting points — the post-stream budget gate and the summarizer
/// compaction path — record into one shared view.
#[derive(Clone, Default)]
pub struct UsageStats {
    entries: Arc<Mutex<HashMap<AgentId, UsageEntry>>>,
}

impl UsageStats {
    /// Record one LLM response's usage for `agent_id`.
    ///
    /// Missing cache-read attribution (`cache_read_tokens: None`) counts as
    /// zero reads, not as a failed record: the hit ratio then simply reads
    /// lower for providers that do not report the field.
    pub fn record(&self, agent_id: &AgentId, usage: &Usage) {
        let mut entries = self.entries.lock().expect("usage stats map poisoned");
        let entry = entries.entry(agent_id.clone()).or_default();
        entry.prompt_tokens += u64::from(usage.prompt_tokens);
        entry.completion_tokens += u64::from(usage.completion_tokens);
        entry.cache_read_tokens += usage.cache_read_tokens.map_or(0, u64::from);
    }

    /// This agent's single-launch totals, or `None` if it recorded nothing.
    pub fn agent(&self, agent_id: AgentId) -> Option<UsageEntry> {
        self.entries
            .lock()
            .expect("usage stats map poisoned")
            .get(&agent_id)
            .copied()
    }

    /// Sums over every recorded agent (single-launch scope).
    pub fn totals(&self) -> UsageTotals {
        let entries = self.entries.lock().expect("usage stats map poisoned");
        entries
            .values()
            .fold(UsageTotals::default(), |t, e| UsageTotals {
                prompt_tokens: t.prompt_tokens + e.prompt_tokens,
                completion_tokens: t.completion_tokens + e.completion_tokens,
                cache_read_tokens: t.cache_read_tokens + e.cache_read_tokens,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u32, completion: u32, cache_read: Option<u32>) -> Usage {
        Usage {
            completion_tokens: completion,
            prompt_tokens: prompt,
            cache_read_tokens: cache_read,
            cache_write_tokens: None,
            total_tokens: prompt + completion,
            completion_tokens_details: None,
        }
    }

    #[test]
    fn record_accumulates_per_agent_and_keeps_agents_independent() {
        let stats = UsageStats::default();
        let a = AgentId::random();
        let b = AgentId::random();
        stats.record(&a, &usage(100, 10, Some(80)));
        stats.record(&a, &usage(50, 5, None));
        stats.record(&b, &usage(200, 20, Some(100)));

        let a_entry = stats.agent(a).expect("agent a recorded");
        assert_eq!(a_entry.prompt_tokens, 150);
        assert_eq!(a_entry.completion_tokens, 15);
        assert_eq!(a_entry.cache_read_tokens, 80);
        let b_entry = stats.agent(b).expect("agent b recorded");
        assert_eq!(b_entry.prompt_tokens, 200);
        assert_eq!(b_entry.cache_read_tokens, 100);
    }

    #[test]
    fn agent_returns_none_before_first_record() {
        let stats = UsageStats::default();
        assert!(stats.agent(AgentId::random()).is_none());
    }

    #[test]
    fn totals_sum_counters_across_agents() {
        let stats = UsageStats::default();
        stats.record(&AgentId::random(), &usage(100, 10, Some(80)));
        stats.record(&AgentId::random(), &usage(50, 5, None));
        let totals = stats.totals();
        assert_eq!(totals.prompt_tokens, 150);
        assert_eq!(totals.completion_tokens, 15);
        assert_eq!(totals.cache_read_tokens, 80);
    }

    #[test]
    fn cache_hit_rate_is_zero_without_prompt_tokens() {
        let stats = UsageStats::default();
        let a = AgentId::random();
        stats.record(&a, &usage(0, 0, None));
        let entry = stats.agent(a).expect("recorded");
        assert_eq!(entry.cache_hit_rate(), 0.0);
        assert_eq!(stats.totals().cache_hit_rate(), 0.0);
    }

    #[test]
    fn totals_ratio_sums_before_dividing_not_ratio_average() {
        let stats = UsageStats::default();
        // Agent A: 900 prompt / 900 read (ratio 1.0); agent B: 100/0 (0.0).
        // Sum-then-divide weighs by prompts: 900/1000 = 0.9. The unweighted
        // ratio average would give (1.0 + 0.0) / 2 = 0.5 — asserting 0.9
        // fails if the totals ever average per-agent ratios instead.
        stats.record(&AgentId::random(), &usage(900, 1, Some(900)));
        stats.record(&AgentId::random(), &usage(100, 1, None));
        let rate = stats.totals().cache_hit_rate();
        assert!((rate - 0.9).abs() < 1e-9);
    }
}
