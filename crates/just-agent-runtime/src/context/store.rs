//! Single source of truth for all context data in an agent.

use std::collections::VecDeque;
use std::ops::Range;

use anyhow::{Result, bail};
use just_agent_common::context::{ContextUsage, CumulativeUsage};
use just_llm_client::types::chat::{ChatMessage, ToolDefinition};

use just_agent_common::retry::RetryRecord;

use super::turn::{Turn, TurnId, estimate_message_tokens};

/// A pinned context item with a label for identification and lifecycle.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PinnedItem {
    pub label: String,
    pub message: ChatMessage,
    /// Cached `estimate_message_tokens(&message)`, computed once on creation (mirrors
    /// `Turn.estimated_tokens`). `#[serde(default)]` so legacy pins (pre-caching) deserialize
    /// as 0 and are backfilled on restore.
    #[serde(default)]
    pub estimated_tokens: usize,
}

/// Result of evicting turns from the context store.
#[derive(Clone, Debug)]
pub struct EvictResult {
    /// Number of turns actually evicted.
    pub evicted: usize,
    /// Turns remaining after eviction.
    pub remaining_turns: usize,
    /// Estimated tokens freed by eviction.
    pub freed_tokens: usize,
}

/// Trait for the agent's context management interface.
///
/// Tools depend on this trait, not on the concrete `ContextStore`.
/// This decouples the tools layer from the context implementation.
pub trait AgenticContext: Send + Sync {
    /// Pin a message with a label. Errors if the label already exists.
    fn pin(&mut self, label: &str, message: ChatMessage) -> Result<()>;
    /// Unpin a message by label. Errors if the label is not found.
    fn unpin(&mut self, label: &str) -> Result<()>;
    /// Atomically replace a pinned item or pin new if label doesn't exist.
    fn replace_pin(&mut self, label: &str, message: ChatMessage) -> Result<()>;
    /// Return the labels of all currently pinned items.
    fn pinned_labels(&self) -> Vec<String>;
    /// Return a snapshot of current context layer breakdown.
    fn usage_snapshot(&self) -> ContextUsage;
    /// Evict the oldest `count` turns. Returns actual count evicted.
    fn evict_turns(&mut self, count: usize) -> EvictResult;
    /// Reset context-window progressive warning state. Called after compaction
    /// or eviction. Does **not** reset token-budget warnings — those track
    /// daemon-wide consumption and must never be reset by a single agent.
    fn reset_context_warnings(&mut self);
}

/// Single source of truth for all context data in an agent.
///
/// Owns tool definitions, pinned messages, and conversation turns.
/// Budget checking is handled by the main loop using ChatClient's
/// accurate token estimation pipeline.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ContextStore {
    /// Tool definitions: reconstructed on restore, not persisted.
    #[serde(skip)]
    tool_definitions: Vec<ToolDefinition>,
    /// Pinned items: always included, never evicted.
    pinned: Vec<PinnedItem>,
    /// Logical conversation turns in chronological order (newest at back).
    turns: VecDeque<Turn>,
    /// Legacy field: migrated to pinned item on restore.
    #[serde(default, skip_serializing)]
    summary: Option<String>,
    /// Legacy field: migrated alongside summary.
    #[serde(default, skip_serializing)]
    summary_tokens: usize,
    /// Exact prompt token count from the last provider response.
    last_prompt_tokens: Option<u32>,
    /// Number of turns baked into `last_prompt_tokens` (the incremental-estimate anchor).
    /// Runtime-only: resets to 0 on restore, forcing a full estimate on the first post-restore
    /// round (see `needs_full_estimate`).
    #[serde(skip, default)]
    anchored_turn_count: usize,
    /// True when the next estimate must be a full render rather than an incremental one anchored
    /// to `last_prompt_tokens`. Set by any prefix-mutating op (evict/drain/pin/unpin/replace),
    /// by failover (the new provider's tokenizer renders the same prompt to a different count),
    /// and — critically — on restore. Cleared by `accumulate_usage`.
    ///
    /// `#[serde(skip)]` defaults to `false`. That is correct for fresh agents: `new()` sets
    /// `true`, and in any case `last_prompt_tokens` starts `None` so the estimator takes the full
    /// branch regardless of this flag. Restored agents get it set to `true` by `restore_agent`,
    /// because a persisted `last_prompt_tokens` is authoritative ONLY for the exact prompt
    /// (system prompt + tools + pinned + turns) that produced it — and a restore may follow an
    /// agent-version upgrade that changed the system prompt or tool set, making the persisted
    /// base stale. The full estimate recomputes from the current config, so the gate never trusts
    /// a cross-version anchor.
    #[serde(skip)]
    needs_full_estimate: bool,
    /// Cumulative token usage across all LLM calls for this agent.
    #[serde(default)]
    cumulative_usage: CumulativeUsage,
    /// The next turn ID to assign.
    next_turn_id: u64,
    /// Historical retry records, persisted across agent restarts.
    ///
    /// Append-only and never pruned: one record per retry attempt accumulates over the agent's
    /// lifetime and across restarts (via `context.json`). The per-endpoint retry budget
    /// (`runner.rs`) only counts records within `retry_timeout` by `timestamp`, so stale entries
    /// don't inflate the budget — but they are not removed from this `Vec`.
    #[serde(default)]
    pub retry_log: Vec<RetryRecord>,
    /// Maximum tokens for the pinned layer. 0 = no limit.
    #[serde(skip)]
    pinned_token_budget: usize,
    /// Highest warning threshold already fired for this agent. Not persisted.
    #[serde(skip)]
    highest_warned_pct: Option<u8>,
    /// Highest token-budget warning threshold already fired. Not persisted.
    #[serde(skip)]
    highest_budget_warned_pct: Option<u8>,
}

impl AgenticContext for ContextStore {
    fn pin(&mut self, label: &str, message: ChatMessage) -> Result<()> {
        if self.pinned.iter().any(|p| p.label == label) {
            bail!("pinned item '{label}' already exists");
        }
        let msg_tokens = estimate_message_tokens(&message);
        let current_pinned = self.pinned_tokens_total();
        if self.pinned_token_budget > 0 && current_pinned + msg_tokens > self.pinned_token_budget {
            bail!(
                "pinned budget exceeded: {current_pinned} + {msg_tokens} > {}. Unpin items to make room.",
                self.pinned_token_budget
            );
        }
        self.pinned.push(PinnedItem {
            label: label.to_owned(),
            estimated_tokens: msg_tokens,
            message,
        });
        self.needs_full_estimate = true;
        Ok(())
    }

    fn unpin(&mut self, label: &str) -> Result<()> {
        let idx = self
            .pinned
            .iter()
            .position(|p| p.label == label)
            .ok_or_else(|| anyhow::anyhow!("pinned item '{label}' not found"))?;
        self.pinned.remove(idx);
        self.needs_full_estimate = true;
        Ok(())
    }

    fn pinned_labels(&self) -> Vec<String> {
        self.pinned.iter().map(|p| p.label.clone()).collect()
    }

    fn usage_snapshot(&self) -> ContextUsage {
        let pinned_items: Vec<(String, usize)> = self
            .pinned
            .iter()
            .map(|p| (p.label.clone(), p.estimated_tokens))
            .collect();
        let turn_tokens: usize = self.turns.iter().map(|t| t.estimated_tokens).sum();
        ContextUsage {
            pinned_items,
            turn_count: self.turns.len(),
            turn_tokens,
            last_prompt_tokens: self.last_prompt_tokens,
            cumulative_usage: self.cumulative_usage,
        }
    }

    fn evict_turns(&mut self, count: usize) -> EvictResult {
        let to_evict = count.min(self.turns.len());
        let freed_tokens: usize = self
            .turns
            .iter()
            .take(to_evict)
            .map(|t| t.estimated_tokens)
            .sum();
        self.turns.drain(0..to_evict);
        self.needs_full_estimate = true;
        EvictResult {
            evicted: to_evict,
            remaining_turns: self.turns.len(),
            freed_tokens,
        }
    }

    fn replace_pin(&mut self, label: &str, message: ChatMessage) -> Result<()> {
        let msg_tokens = estimate_message_tokens(&message);
        let existing_idx = self.pinned.iter().position(|p| p.label == label);
        let old_tokens = existing_idx
            .map(|i| self.pinned[i].estimated_tokens)
            .unwrap_or(0);
        let base_tokens = self.pinned_tokens_total() - old_tokens;

        if self.pinned_token_budget > 0 && base_tokens + msg_tokens > self.pinned_token_budget {
            bail!(
                "pinned budget exceeded after replace: {} > {}. Unpin other items to make room.",
                base_tokens + msg_tokens,
                self.pinned_token_budget
            );
        }

        if let Some(idx) = existing_idx {
            self.pinned[idx].message = message;
            self.pinned[idx].estimated_tokens = msg_tokens;
        } else {
            self.pinned.push(PinnedItem {
                label: label.to_owned(),
                estimated_tokens: msg_tokens,
                message,
            });
        }
        self.needs_full_estimate = true;
        Ok(())
    }

    fn reset_context_warnings(&mut self) {
        self.highest_warned_pct = None;
    }
}

impl Default for ContextStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextStore {
    pub fn new() -> Self {
        Self {
            tool_definitions: Vec::new(),
            pinned: Vec::new(),
            turns: VecDeque::new(),
            summary: None,
            summary_tokens: 0,
            last_prompt_tokens: None,
            anchored_turn_count: 0,
            needs_full_estimate: true,
            cumulative_usage: CumulativeUsage::default(),
            next_turn_id: 0,
            retry_log: Vec::new(),
            pinned_token_budget: 0,
            highest_warned_pct: None,
            highest_budget_warned_pct: None,
        }
    }

    /// Store tool definitions. Called once after tool registration.
    pub fn set_tool_definitions(&mut self, defs: Vec<ToolDefinition>) {
        self.tool_definitions = defs;
    }

    /// Access the stored tool definitions.
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        &self.tool_definitions
    }

    /// Accumulate exact token usage from a **main-conversation** provider response: bumps
    /// `cumulative_usage`, records the authoritative `last_prompt_tokens`, and re-anchors the
    /// incremental estimate to the current turn count (clearing `needs_full_estimate`).
    ///
    /// For LLM calls whose usage does NOT reflect the main conversation prompt (e.g. the
    /// summarizer, which runs over a different message set), use
    /// [`accumulate_usage_no_anchor`](Self::accumulate_usage_no_anchor) — those must not move
    /// the anchor.
    pub fn accumulate_usage(&mut self, usage: &just_llm_client::types::chat::Usage) {
        self.cumulative_usage.prompt_tokens += usage.prompt_tokens as u64;
        self.cumulative_usage.completion_tokens += usage.completion_tokens as u64;
        if let Some(hit) = usage.prompt_cache_hit_tokens {
            self.cumulative_usage.cache_hit_tokens += hit as u64;
        }
        self.last_prompt_tokens = Some(usage.prompt_tokens);
        self.anchored_turn_count = self.turns.len();
        self.needs_full_estimate = false;
    }

    /// Accumulate usage for a non-main-conversation call (e.g. the summarizer): bumps
    /// `cumulative_usage` only, leaving `last_prompt_tokens` / `anchored_turn_count` /
    /// `needs_full_estimate` untouched so the main prompt anchor is not poisoned.
    pub fn accumulate_usage_no_anchor(&mut self, usage: &just_llm_client::types::chat::Usage) {
        self.cumulative_usage.prompt_tokens += usage.prompt_tokens as u64;
        self.cumulative_usage.completion_tokens += usage.completion_tokens as u64;
        if let Some(hit) = usage.prompt_cache_hit_tokens {
            self.cumulative_usage.cache_hit_tokens += hit as u64;
        }
    }

    /// The authoritative prompt-token count from the last main-conversation response, if any.
    pub fn last_prompt_tokens(&self) -> Option<u32> {
        self.last_prompt_tokens
    }

    /// Turns baked into `last_prompt_tokens` (the incremental-estimate anchor).
    pub fn anchored_turn_count(&self) -> usize {
        self.anchored_turn_count
    }

    /// Whether the next estimate must be a full render (a prefix-mutating op occurred).
    pub fn needs_full_estimate(&self) -> bool {
        self.needs_full_estimate
    }

    /// Mark that the prefix baked into `last_prompt_tokens` has changed (evict/drain/pin/unpin/
    /// replace/failover), forcing the next estimate into full mode until a response re-anchors.
    pub(crate) fn mark_needs_full_estimate(&mut self) {
        self.needs_full_estimate = true;
    }

    /// Returns the cumulative token usage snapshot.
    pub fn cumulative_usage(&self) -> &CumulativeUsage {
        &self.cumulative_usage
    }

    /// Append a new turn from the given messages.
    /// Returns the assigned turn ID and the estimated token count.
    pub fn push_turn(&mut self, messages: Vec<ChatMessage>) -> (TurnId, usize) {
        let estimated_tokens = Turn::estimate_tokens(&messages);
        let id = TurnId(self.next_turn_id);
        self.next_turn_id += 1;
        self.turns.push_back(Turn {
            id,
            messages,
            estimated_tokens,
        });
        (id, estimated_tokens)
    }

    /// Immutable access to the pinned items.
    pub fn pinned(&self) -> &[PinnedItem] {
        &self.pinned
    }

    /// Immutable access to conversation turns.
    pub fn turns(&self) -> &VecDeque<Turn> {
        &self.turns
    }

    /// Number of turns stored.
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Remove turns in the given range and return them.
    pub fn drain_turns(&mut self, range: Range<usize>) -> Vec<Turn> {
        let drained = self.turns.drain(range).collect();
        self.needs_full_estimate = true;
        drained
    }

    /// Migrate legacy `summary` field to a pinned item.
    /// Called during agent restore. No-op if no legacy summary.
    pub fn migrate_legacy_summary(&mut self) {
        if let Some(summary) = self.summary.take() {
            if !summary.is_empty() {
                self.unpin("context_summary").ok();
                self.pin("context_summary", ChatMessage::assistant(&summary))
                    .ok();
                tracing::info!("migrated legacy summary to pinned item");
            }
            self.summary_tokens = 0;
        }
    }

    /// Backfill `estimated_tokens` for pinned items deserialized from a pre-caching format (which
    /// default to 0 via `#[serde(default)]`). Called on restore, before any budget computation or
    /// migration reads the cache. Idempotent: a real pinned message always estimates to ≥16 (the
    /// per-message overhead), so `== 0` identifies only legacy uncached entries.
    pub fn backfill_pinned_token_cache(&mut self) {
        for p in &mut self.pinned {
            if p.estimated_tokens == 0 {
                p.estimated_tokens = estimate_message_tokens(&p.message);
            }
        }
    }

    /// Set the pinned token budget. Called at agent setup and re-synced on within-tier failover
    /// (see `runner::reapply_window`).
    pub fn set_pinned_budget(&mut self, budget: usize) {
        self.pinned_token_budget = budget;
    }

    /// Sum estimated tokens across all pinned items (reads the cached `estimated_tokens`).
    pub fn pinned_tokens_total(&self) -> usize {
        self.pinned.iter().map(|p| p.estimated_tokens).sum()
    }

    /// Total estimated tokens: pinned items + all turns.
    pub fn total_estimated_tokens(&self) -> usize {
        self.pinned_tokens_total() + self.turns.iter().map(|t| t.estimated_tokens).sum::<usize>()
    }

    /// Check if a warning at the given threshold should fire.
    pub fn should_warn(&self, threshold_pct: u8) -> bool {
        self.highest_warned_pct
            .is_none_or(|prev| threshold_pct > prev)
    }

    /// Record that a warning has been fired at the given threshold.
    pub fn mark_warned(&mut self, pct: u8) {
        self.highest_warned_pct = Some(self.highest_warned_pct.unwrap_or(0).max(pct));
    }

    /// Check if a token-budget warning at the given threshold should fire.
    pub fn should_warn_budget(&self, threshold_pct: u8) -> bool {
        self.highest_budget_warned_pct
            .is_none_or(|prev| threshold_pct > prev)
    }

    /// Record that a token-budget warning has been fired at the given threshold.
    pub fn mark_budget_warned(&mut self, pct: u8) {
        self.highest_budget_warned_pct = Some(self.highest_budget_warned_pct.unwrap_or(0).max(pct));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_store() -> ContextStore {
        ContextStore::new()
    }

    #[test]
    fn push_turn_assigns_sequential_ids() {
        let mut store = new_store();
        let (id0, _) = store.push_turn(vec![ChatMessage::user("a")]);
        let (id1, _) = store.push_turn(vec![ChatMessage::user("b")]);
        assert_eq!(id0, TurnId(0));
        assert_eq!(id1, TurnId(1));
        assert_eq!(store.turn_count(), 2);
    }

    #[test]
    fn drain_turns_removes_correct_range() {
        let mut store = new_store();
        store.push_turn(vec![ChatMessage::user("a")]);
        store.push_turn(vec![ChatMessage::user("b")]);
        store.push_turn(vec![ChatMessage::user("c")]);

        let drained = store.drain_turns(0..2);
        assert_eq!(drained.len(), 2);
        assert_eq!(store.turn_count(), 1);
    }

    #[test]
    fn pinned_items_are_tracked() {
        let mut store = new_store();
        store.pin("test", ChatMessage::user("important")).unwrap();
        assert_eq!(store.pinned().len(), 1);
        assert_eq!(store.pinned_labels(), vec!["test"]);
    }

    #[test]
    fn pin_rejects_duplicate_label() {
        let mut store = new_store();
        store.pin("x", ChatMessage::user("a")).unwrap();
        assert!(store.pin("x", ChatMessage::user("b")).is_err());
    }

    #[test]
    fn unpin_removes_item() {
        let mut store = new_store();
        store.pin("x", ChatMessage::user("a")).unwrap();
        store.unpin("x").unwrap();
        assert!(store.pinned().is_empty());
    }

    #[test]
    fn unpin_errors_on_missing_label() {
        let mut store = new_store();
        assert!(store.unpin("nonexistent").is_err());
    }

    #[test]
    fn pinned_budget_enforced() {
        let mut store = ContextStore::new();
        store.set_pinned_budget(80);
        // Each ChatMessage::user("a") estimates to 16 tokens (1/4 + 16).
        store.pin("a", ChatMessage::user("a")).unwrap();
        store.pin("b", ChatMessage::user("b")).unwrap();
        store.pin("c", ChatMessage::user("c")).unwrap();
        store.pin("d", ChatMessage::user("d")).unwrap();
        store.pin("e", ChatMessage::user("e")).unwrap();
        // Sixth pin exceeds 80-token budget (96 > 80).
        assert!(store.pin("f", ChatMessage::user("f")).is_err());
        // Unpin frees budget.
        store.unpin("a").unwrap();
        assert!(store.pin("f", ChatMessage::user("f")).is_ok());
    }

    #[test]
    fn warning_tracking() {
        let mut store = ContextStore::new();
        assert!(store.should_warn(50));
        assert!(store.should_warn(60));
        store.mark_warned(50);
        assert!(!store.should_warn(50));
        assert!(store.should_warn(60));
        store.mark_warned(60);
        assert!(!store.should_warn(50));
        assert!(!store.should_warn(60));
        assert!(store.should_warn(70));
        store.reset_context_warnings();
        assert!(store.should_warn(50));
    }

    // --- incremental-estimate anchor / flag mechanics ---

    fn usage(prompt_tokens: u32) -> just_llm_client::types::chat::Usage {
        just_llm_client::types::chat::Usage {
            prompt_tokens,
            completion_tokens: 0,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
            total_tokens: prompt_tokens,
            completion_tokens_details: None,
        }
    }

    #[test]
    fn new_store_starts_in_full_mode() {
        let store = new_store();
        assert!(store.needs_full_estimate());
        assert_eq!(store.anchored_turn_count(), 0);
        assert_eq!(store.last_prompt_tokens(), None);
    }

    #[test]
    fn accumulate_usage_sets_anchor_and_clears_flag() {
        let mut store = new_store();
        store.push_turn(vec![ChatMessage::user("a")]);
        store.push_turn(vec![ChatMessage::user("b")]);
        assert!(store.needs_full_estimate(), "new store starts in full mode");
        store.accumulate_usage(&usage(100));
        assert_eq!(store.last_prompt_tokens(), Some(100));
        assert_eq!(
            store.anchored_turn_count(),
            2,
            "anchor = turns at response time"
        );
        assert!(!store.needs_full_estimate());
    }

    #[test]
    fn prefix_ops_set_needs_full_estimate() {
        let clear = |s: &mut ContextStore| s.accumulate_usage(&usage(1));
        let mut store = new_store();

        clear(&mut store);
        store.pin("x", ChatMessage::user("a")).unwrap();
        assert!(store.needs_full_estimate(), "pin sets the flag");

        clear(&mut store);
        store.unpin("x").unwrap();
        assert!(store.needs_full_estimate(), "unpin sets the flag");

        clear(&mut store);
        store.replace_pin("y", ChatMessage::user("c")).unwrap();
        assert!(store.needs_full_estimate(), "replace_pin sets the flag");

        store.push_turn(vec![ChatMessage::user("t1")]);
        store.push_turn(vec![ChatMessage::user("t2")]);
        clear(&mut store);
        store.evict_turns(1);
        assert!(store.needs_full_estimate(), "evict_turns sets the flag");

        clear(&mut store);
        store.drain_turns(0..1);
        assert!(store.needs_full_estimate(), "drain_turns sets the flag");
    }

    #[test]
    fn accumulate_usage_no_anchor_leaves_anchor_untouched() {
        let mut store = new_store();
        store.push_turn(vec![ChatMessage::user("a")]);
        store.accumulate_usage(&usage(100)); // anchor at 1 turn, base 100
        assert_eq!(store.last_prompt_tokens(), Some(100));
        assert_eq!(store.anchored_turn_count(), 1);
        assert!(!store.needs_full_estimate());

        // A summarizer-style call must not move the anchor, only grow cumulative usage.
        let prev_cumulative = store.cumulative_usage().prompt_tokens;
        store.accumulate_usage_no_anchor(&usage(50));
        assert_eq!(store.last_prompt_tokens(), Some(100), "base unchanged");
        assert_eq!(store.anchored_turn_count(), 1, "anchor unchanged");
        assert!(!store.needs_full_estimate(), "flag unchanged");
        assert_eq!(
            store.cumulative_usage().prompt_tokens,
            prev_cumulative + 50,
            "cumulative usage still grows"
        );
    }

    // --- PinnedItem token caching ---

    #[test]
    fn pinned_item_caches_estimated_tokens() {
        let mut store = new_store();
        let msg = ChatMessage::user("hello world");
        let expected = estimate_message_tokens(&msg);
        store.pin("x", msg).unwrap();
        assert_eq!(store.pinned()[0].estimated_tokens, expected);
        assert_eq!(store.pinned_tokens_total(), expected);
        // replace_pin updates the cache.
        let msg2 = ChatMessage::user("goodbye world and more content here");
        let expected2 = estimate_message_tokens(&msg2);
        store.replace_pin("x", msg2).unwrap();
        assert_eq!(store.pinned()[0].estimated_tokens, expected2);
    }

    #[test]
    fn pinned_token_cache_backfilled() {
        let mut store = new_store();
        // Simulate a legacy pin deserialized without the cache (estimated_tokens == 0).
        let msg = ChatMessage::user("legacy content from a pre-caching format");
        let real = estimate_message_tokens(&msg);
        store.pinned.push(PinnedItem {
            label: "legacy".into(),
            message: msg,
            estimated_tokens: 0,
        });
        assert_eq!(
            store.pinned_tokens_total(),
            0,
            "legacy pin reads as 0 before backfill"
        );
        store.backfill_pinned_token_cache();
        assert_eq!(
            store.pinned_tokens_total(),
            real,
            "backfill recomputes the cache"
        );
    }

    #[test]
    fn pinned_item_estimated_tokens_serde_default_is_zero() {
        // New-format pin round-trips with its cached value.
        let item = PinnedItem {
            label: "x".into(),
            message: ChatMessage::user("hi"),
            estimated_tokens: 42,
        };
        let json = serde_json::to_string(&item).unwrap();
        let rt: PinnedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.estimated_tokens, 42);

        // Strip the field to emulate a legacy (pre-caching) serialized pin.
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("estimated_tokens");
        let legacy = serde_json::to_string(&v).unwrap();
        let legacy_rt: PinnedItem = serde_json::from_str(&legacy).unwrap();
        assert_eq!(legacy_rt.estimated_tokens, 0, "missing field defaults to 0");
    }
}
