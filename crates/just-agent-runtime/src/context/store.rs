//! Single source of truth for all context data in an agent.

use std::collections::VecDeque;
use std::ops::Range;

use anyhow::{Result, bail};
use just_agent_common::context::{ContextUsage, CumulativeUsage};
use just_llm_client::types::chat::{ChatMessage, ToolDefinition};

use just_agent_common::retry::RetryRecord;

use super::turn::{Turn, TurnId, TurnKind, estimate_message_tokens};

/// Legacy pinned-item shape from the pre-unification format. Deserialized from old `context.json`
/// `pinned` entries and converted to pinned [`Turn`]s by [`ContextStore::migrate_legacy_pinned`]
/// on restore. Not constructed by new code.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PinnedItem {
    pub label: String,
    pub message: ChatMessage,
    /// Cached `estimate_message_tokens(&message)`. `#[serde(default)]` so legacy pins
    /// (pre-caching) deserialize as 0 and are backfilled on restore.
    #[serde(default)]
    pub estimated_tokens: usize,
}

/// Result of evicting turns from the context store.
#[derive(Clone, Debug)]
pub struct EvictResult {
    /// Number of turns actually evicted.
    pub evicted: usize,
    /// Conversation (non-pinned) turns remaining after eviction.
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
    /// Evict the oldest `count` conversation (non-pinned) turns. Returns actual count evicted.
    fn evict_turns(&mut self, count: usize) -> EvictResult;
    /// Reset context-window progressive warning state. Called after compaction
    /// or eviction. Does **not** reset token-budget warnings — those track
    /// daemon-wide consumption and must never be reset by a single agent.
    fn reset_context_warnings(&mut self);
}

/// Single source of truth for all context data in an agent.
///
/// Owns tool definitions and conversation turns. Pinned persistent context (compaction
/// summaries, skills, notes) is stored as `TurnKind::Pinned` turns at the front of `turns`,
/// keeping a single collection. Budget checking is handled by the main loop using ChatClient's
/// accurate token estimation pipeline.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ContextStore {
    /// Tool definitions: reconstructed on restore, not persisted.
    #[serde(skip)]
    tool_definitions: Vec<ToolDefinition>,
    /// Logical conversation turns in chronological order. Always ordered
    /// `[pinned turns…][conversation turns…]` (see [`Self::pinned_turn_count`]); pinned turns
    /// live at the front and are never evicted.
    turns: VecDeque<Turn>,
    /// Legacy pinned items from the pre-unification format. Deserialized from the old `pinned`
    /// JSON key and folded into pinned turns by [`Self::migrate_legacy_pinned`] on restore.
    /// Never written by new code (`skip_serializing`).
    #[serde(default, skip_serializing, rename = "pinned")]
    legacy_pinned: Vec<PinnedItem>,
    /// Legacy field: migrated to a pinned turn on restore.
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
        if self.pinned_turns().any(|t| t.label() == Some(label)) {
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
        let id = TurnId(self.next_turn_id);
        self.next_turn_id += 1;
        self.turns.insert(
            self.pinned_turn_count(),
            Turn {
                id,
                messages: vec![message],
                estimated_tokens: msg_tokens,
                kind: TurnKind::Pinned {
                    label: label.to_owned(),
                },
            },
        );
        self.needs_full_estimate = true;
        Ok(())
    }

    fn unpin(&mut self, label: &str) -> Result<()> {
        let idx = self
            .turns
            .iter()
            .position(|t| t.is_pinned() && t.label() == Some(label))
            .ok_or_else(|| anyhow::anyhow!("pinned item '{label}' not found"))?;
        self.turns.remove(idx);
        self.needs_full_estimate = true;
        Ok(())
    }

    fn pinned_labels(&self) -> Vec<String> {
        self.pinned_turns()
            .filter_map(|t| t.label().map(str::to_owned))
            .collect()
    }

    fn usage_snapshot(&self) -> ContextUsage {
        let mut pinned_items = Vec::new();
        let mut turn_count = 0usize;
        let mut turn_tokens = 0usize;
        for turn in &self.turns {
            if turn.is_pinned() {
                if let Some(label) = turn.label() {
                    pinned_items.push((label.to_owned(), turn.estimated_tokens));
                }
            } else {
                turn_count += 1;
                turn_tokens += turn.estimated_tokens;
            }
        }
        ContextUsage {
            pinned_items,
            turn_count,
            turn_tokens,
            last_prompt_tokens: self.last_prompt_tokens,
            cumulative_usage: self.cumulative_usage,
        }
    }

    fn evict_turns(&mut self, count: usize) -> EvictResult {
        // Evict only from the conversation partition (the suffix after the pinned block).
        // Pinned turns at the front are structurally outside the drain range.
        let pinned = self.pinned_turn_count();
        let convo_len = self.turns.len().saturating_sub(pinned);
        let to_evict = count.min(convo_len);
        let freed_tokens: usize = self
            .turns
            .iter()
            .skip(pinned)
            .take(to_evict)
            .map(|t| t.estimated_tokens)
            .sum();
        self.turns.drain(pinned..pinned + to_evict);
        self.needs_full_estimate = true;
        EvictResult {
            evicted: to_evict,
            remaining_turns: self.turns.len().saturating_sub(self.pinned_turn_count()),
            freed_tokens,
        }
    }

    fn replace_pin(&mut self, label: &str, message: ChatMessage) -> Result<()> {
        let msg_tokens = estimate_message_tokens(&message);
        let existing_idx = self
            .turns
            .iter()
            .position(|t| t.is_pinned() && t.label() == Some(label));
        let old_tokens = existing_idx
            .map(|i| self.turns[i].estimated_tokens)
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
            // In-place update keeps the turn's position and id; only content/tokens change.
            self.turns[idx].messages = vec![message];
            self.turns[idx].estimated_tokens = msg_tokens;
        } else {
            let id = TurnId(self.next_turn_id);
            self.next_turn_id += 1;
            self.turns.insert(
                self.pinned_turn_count(),
                Turn {
                    id,
                    messages: vec![message],
                    estimated_tokens: msg_tokens,
                    kind: TurnKind::Pinned {
                        label: label.to_owned(),
                    },
                },
            );
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
            turns: VecDeque::new(),
            legacy_pinned: Vec::new(),
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

    /// Append a new conversation turn from the given messages.
    /// Returns the assigned turn ID and the estimated token count.
    pub fn push_turn(&mut self, messages: Vec<ChatMessage>) -> (TurnId, usize) {
        let estimated_tokens = Turn::estimate_tokens(&messages);
        let id = TurnId(self.next_turn_id);
        self.next_turn_id += 1;
        self.turns.push_back(Turn {
            id,
            messages,
            estimated_tokens,
            kind: TurnKind::Conversation,
        });
        (id, estimated_tokens)
    }

    /// Number of pinned turns at the front of `turns` (the partition split index). O(pinned).
    /// Relies on the `[pinned…][conversation…]` ordering invariant.
    fn pinned_turn_count(&self) -> usize {
        self.turns.iter().take_while(|t| t.is_pinned()).count()
    }

    /// Iterator over the pinned turns.
    pub fn pinned_turns(&self) -> impl Iterator<Item = &Turn> {
        self.turns.iter().filter(|t| t.is_pinned())
    }

    /// Immutable access to all turns (pinned first, then conversation).
    pub fn turns(&self) -> &VecDeque<Turn> {
        &self.turns
    }

    /// Total number of turns stored (pinned + conversation).
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Remove turns in the given range and return them.
    pub fn drain_turns(&mut self, range: Range<usize>) -> Vec<Turn> {
        let drained = self.turns.drain(range).collect();
        self.needs_full_estimate = true;
        drained
    }

    /// Migrate legacy `summary` field to a pinned turn.
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

    /// Backfill `estimated_tokens` for legacy pinned items deserialized from a pre-caching format
    /// (which default to 0 via `#[serde(default)]`). Targets the legacy `pinned` vec, before
    /// [`Self::migrate_legacy_pinned`] folds them into turns. Idempotent.
    pub fn backfill_pinned_token_cache(&mut self) {
        for p in &mut self.legacy_pinned {
            if p.estimated_tokens == 0 {
                p.estimated_tokens = estimate_message_tokens(&p.message);
            }
        }
    }

    /// Fold the legacy `pinned` vec (pre-unification format) into pinned turns at the front of
    /// `turns`. Called on restore after [`Self::backfill_pinned_token_cache`]. No-op for new-format
    /// stores (the legacy vec is empty). Preserves legacy order; TurnIds are assigned monotonic
    /// from `next_turn_id`.
    pub fn migrate_legacy_pinned(&mut self) {
        let legacy = std::mem::take(&mut self.legacy_pinned);
        let mut inserted = 0usize;
        for item in legacy {
            let id = TurnId(self.next_turn_id);
            self.next_turn_id += 1;
            self.turns.insert(
                inserted,
                Turn {
                    id,
                    messages: vec![item.message],
                    estimated_tokens: item.estimated_tokens,
                    kind: TurnKind::Pinned { label: item.label },
                },
            );
            inserted += 1;
        }
        if inserted > 0 {
            self.needs_full_estimate = true;
        }
    }

    /// Set the pinned token budget. Called at agent setup and re-synced on within-tier failover
    /// (see `runner::reapply_window`).
    pub fn set_pinned_budget(&mut self, budget: usize) {
        self.pinned_token_budget = budget;
    }

    /// Sum estimated tokens across all pinned turns (reads the cached `estimated_tokens`).
    pub fn pinned_tokens_total(&self) -> usize {
        self.pinned_turns().map(|t| t.estimated_tokens).sum()
    }

    /// Total estimated tokens across all turns (pinned + conversation).
    pub fn total_estimated_tokens(&self) -> usize {
        self.turns.iter().map(|t| t.estimated_tokens).sum()
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

    /// Assert the `[pinned…][conversation…]` ordering invariant and that every pinned turn has a
    /// label. Called at the end of mutating tests.
    fn assert_invariant(store: &ContextStore) {
        let mut seen_conversation = false;
        for t in store.turns() {
            if t.is_pinned() {
                assert!(t.label().is_some(), "pinned turn must have a label");
                assert!(
                    !seen_conversation,
                    "pinned turn must precede all conversation turns"
                );
            } else {
                seen_conversation = true;
            }
        }
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
        assert_eq!(store.pinned_turns().count(), 1);
        assert_eq!(store.pinned_labels(), vec!["test"]);
        assert_invariant(&store);
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
        assert_eq!(store.pinned_turns().count(), 0);
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

    // --- pinned-turn token caching ---

    #[test]
    fn pinned_turn_caches_estimated_tokens() {
        let mut store = new_store();
        let msg = ChatMessage::user("hello world");
        let expected = estimate_message_tokens(&msg);
        store.pin("x", msg).unwrap();
        let pinned = store.pinned_turns().next().unwrap();
        assert_eq!(pinned.estimated_tokens, expected);
        assert_eq!(store.pinned_tokens_total(), expected);
        // replace_pin updates the cache in place.
        let msg2 = ChatMessage::user("goodbye world and more content here");
        let expected2 = estimate_message_tokens(&msg2);
        store.replace_pin("x", msg2).unwrap();
        let pinned = store.pinned_turns().next().unwrap();
        assert_eq!(pinned.estimated_tokens, expected2);
        assert_invariant(&store);
    }

    #[test]
    fn backfill_then_migrate_legacy_pinned() {
        let mut store = new_store();
        // Simulate a legacy pin deserialized without the cache (estimated_tokens == 0).
        let msg = ChatMessage::user("legacy content from a pre-caching format");
        let real = estimate_message_tokens(&msg);
        store.legacy_pinned.push(PinnedItem {
            label: "legacy".into(),
            message: msg,
            estimated_tokens: 0,
        });
        assert_eq!(
            store.pinned_tokens_total(),
            0,
            "legacy pin not yet folded into turns"
        );
        store.backfill_pinned_token_cache();
        store.migrate_legacy_pinned();
        assert_eq!(store.pinned_turns().count(), 1);
        assert_eq!(
            store.pinned_tokens_total(),
            real,
            "backfill + migrate recomputes"
        );
        assert_eq!(store.pinned_labels(), vec!["legacy"]);
        assert_invariant(&store);
    }

    #[test]
    fn migrate_legacy_pinned_preserves_order_and_ids() {
        let mut store = new_store();
        // Two conversation turns already in the store.
        store.push_turn(vec![ChatMessage::user("c1")]);
        store.push_turn(vec![ChatMessage::user("c2")]);
        let base_next = store.next_turn_id;
        // Inject three legacy pinned items in a known order.
        for label in ["sum", "skill:foo", "note"] {
            store.legacy_pinned.push(PinnedItem {
                label: label.into(),
                message: ChatMessage::user(label),
                estimated_tokens: 5,
            });
        }
        store.migrate_legacy_pinned();

        // Pinned turns at front in original order, conversation turns after.
        let labels: Vec<&str> = store.turns().iter().filter_map(|t| t.label()).collect();
        assert_eq!(labels, vec!["sum", "skill:foo", "note"]);
        assert_eq!(store.turn_count(), 5);
        // Conversation turns still at the back.
        assert_eq!(store.pinned_turn_count(), 3);
        // TurnIds unique and advanced past the migrated block.
        let ids: std::collections::HashSet<TurnId> = store.turns().iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), 5, "all turn ids unique");
        assert_eq!(store.next_turn_id, base_next + 3);
        assert_invariant(&store);
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

    // --- F1/F3 safety: eviction skips pinned; ordering invariants ---

    #[test]
    fn evict_turns_skips_pinned() {
        let mut store = new_store();
        store
            .pin("context_summary", ChatMessage::assistant("sum"))
            .unwrap();
        store.push_turn(vec![ChatMessage::user("c1")]);
        store.push_turn(vec![ChatMessage::user("c2")]);
        store.push_turn(vec![ChatMessage::user("c3")]);

        let res = store.evict_turns(3);
        assert_eq!(res.evicted, 3, "all conversation turns evicted");
        assert_eq!(res.remaining_turns, 0, "no conversation turns remain");
        assert_eq!(
            store.pinned_turns().count(),
            1,
            "pinned summary survives eviction"
        );
        assert_eq!(store.pinned_labels(), vec!["context_summary"]);
        assert_eq!(store.turn_count(), 1, "only the pinned turn remains");

        // Over-evict: pinned still survives, evicted capped at conversation count (already 0).
        let res = store.evict_turns(99);
        assert_eq!(res.evicted, 0);
        assert_eq!(store.pinned_turns().count(), 1);
        assert_invariant(&store);
    }

    #[test]
    fn pin_inserts_after_pinned_partition() {
        let mut store = new_store();
        store.pin("a", ChatMessage::user("a")).unwrap();
        store.push_turn(vec![ChatMessage::user("convo")]);
        store.pin("b", ChatMessage::user("b")).unwrap();
        // Ordering: [a(pinned), b(pinned), convo] — b inserted after the pinned block, not at back.
        let labels: Vec<Option<&str>> = store.turns().iter().map(|t| t.label()).collect();
        assert_eq!(
            labels,
            vec![Some("a"), Some("b"), None],
            "pinned turns stay before conversation turns"
        );
        assert_invariant(&store);
    }

    #[test]
    fn replace_pin_updates_in_place_keeps_position() {
        let mut store = new_store();
        store.pin("a", ChatMessage::user("a-original")).unwrap();
        store.pin("b", ChatMessage::user("b")).unwrap();
        store.push_turn(vec![ChatMessage::user("convo")]);

        let new_msg = ChatMessage::user("a-replaced-longer-content");
        let new_tokens = estimate_message_tokens(&new_msg);
        store.replace_pin("a", new_msg).unwrap();

        let first = &store.turns()[0];
        assert_eq!(first.label(), Some("a"), "position unchanged");
        assert_eq!(first.estimated_tokens, new_tokens);
        // b and convo remain in place.
        assert_eq!(store.turns()[1].label(), Some("b"));
        assert!(!store.turns()[2].is_pinned());
        assert_invariant(&store);
    }
}
