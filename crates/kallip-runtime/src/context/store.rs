//! Single source of truth for all context data in an agent.

use std::collections::{HashSet, VecDeque};
use std::ops::Range;

use anyhow::{Result, bail};
use just_llm_client::types::chat::{ChatMessage, ToolDefinition};
use kallip_common::context::{ContextUsage, CumulativeUsage};

use kallip_common::retry::RetryRecord;

use super::manifest::{FORMAT_VERSION, ManifestDoc, PinRecord, PinsDoc};
use super::tokens::estimate_message_tokens;
use super::turn::{Turn, TurnId, TurnKind};

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
    /// tagma-wide consumption and must never be reset by a single agent.
    fn reset_context_warnings(&mut self);
    /// Return the most recent message of `role` in the **conversation**
    /// (non-pinned) turns, scanning newest-first.
    ///
    /// Pinned turns are deliberately skipped: they live at the front of the
    /// store and include assistant-role entries (notably the compaction
    /// `context_summary`), so a naive reverse scan would resolve `assistant`
    /// to the summary rather than the agent's actual last reply. For the
    /// `assistant` role, pure tool-call dispatch messages (no preamble text,
    /// i.e. `content: None`) are also skipped: they carry nothing to pin, so
    /// the accessor resolves to the last assistant message that has text
    /// content. Tool results always carry content and are never pruned.
    /// Returns `None` when no qualifying conversation message exists.
    fn last_conversation_message_by_role(&self, role: &str) -> Option<ChatMessage>;
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
    /// Turn IDs injected by restore (restart notices) rather than recorded
    /// through `record_turn`. A restart notice is an on-the-spot prompt: it
    /// is meaningless across restarts (the next restore injects a fresh
    /// one), and it has no history record to hydrate from. The manifest
    /// projection skips these IDs; the assigned numbers are still consumed,
    /// so `next_turn_id` stays monotonic.
    #[serde(skip)]
    injected_turn_ids: HashSet<u64>,
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

    fn last_conversation_message_by_role(&self, role: &str) -> Option<ChatMessage> {
        // Skip pinned turns (front of the deque). The compaction summary is an
        // assistant-role pinned message, so including pinned turns would make
        // `assistant` resolve to the summary instead of the last real reply.
        self.turns
            .iter()
            .rev()
            .filter(|t| !t.is_pinned())
            .flat_map(|t| t.messages.iter().rev())
            .find(|m| {
                m.role() == role
                    // A pure tool-call dispatch (no preamble text) carries no
                    // content to pin -- skip it so `assistant` resolves to a
                    // real reply. Tool results always carry content (their
                    // body), so this prunes only assistant dispatch headers
                    // produced by tool-call execution.
                    && (role != "assistant" || m.content().is_some())
            })
            .cloned()
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
            injected_turn_ids: HashSet::new(),
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
    /// Mutable access to all turns. Restore never rewrites hydrated
    /// messages; this exists for the offline repair path
    /// (`persistence::repair_agent_context`), which fixes pairing damage
    /// in memory before appending the repaired record to history.
    pub(crate) fn turns_mut(&mut self) -> &mut VecDeque<Turn> {
        &mut self.turns
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

    /// Recompute every cached token estimate (all turns) via the current estimator. Called once
    /// per restore so persisted estimates — which may be from a prior estimator version (e.g.
    /// the old `char/4` heuristic, or legacy pins with unset caches) — are brought up to date.
    ///
    /// O(turns): every turn is re-rendered and re-scored. A future optimization could gate this
    /// on a persisted estimator-version stamp so same-version restarts skip it; for now the cost
    /// is paid once per restore.
    pub fn reestimate_cached_tokens(&mut self) {
        for turn in &mut self.turns {
            turn.estimated_tokens = Turn::estimate_tokens(&turn.messages);
        }
    }

    /// Fold the legacy `pinned` vec (pre-unification format) into pinned turns at the front of
    /// `turns`. Called on restore before [`Self::reestimate_cached_tokens`]. No-op for new-format
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

    /// Rebuild a store from its split-persistence projection: pinned records
    /// (composition order), hydrated conversation turns, and the manifest's
    /// small state. Inverse of `to_manifest_doc`/`to_pins_doc`.
    ///
    /// `next_turn_id` takes the manifest value but never drops below one past
    /// the highest rehydrated ID — a stale manifest must not cause ID reuse.
    pub(crate) fn from_persisted(pins: &PinsDoc, convo: Vec<Turn>, manifest: &ManifestDoc) -> Self {
        let mut store = Self::new();
        for p in &pins.pins {
            store.turns.push_back(Turn {
                id: TurnId(p.id),
                messages: vec![p.message.clone()],
                estimated_tokens: p.estimated_tokens,
                kind: TurnKind::Pinned {
                    label: p.label.clone(),
                },
            });
        }
        for t in convo {
            store.turns.push_back(t);
        }
        let max_id = store.turns.back().map(|t| t.id.0 + 1).unwrap_or(0);
        store.next_turn_id = manifest.next_turn_id.max(max_id);
        store.cumulative_usage = manifest.cumulative_usage;
        store.retry_log = manifest.retry_log.clone();
        store
    }
    /// Set the pinned token budget. Called at agent setup and re-synced on within-tier failover
    /// (see `acquisition::reapply_window`).
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
    /// Record an injected turn's ID so the manifest projection skips it.
    /// Called by restore when it pushes a restart notice outside
    /// `record_turn`.
    pub fn register_injected_turn(&mut self, id: TurnId) {
        self.injected_turn_ids.insert(id.0);
    }

    /// Project the persistable state onto the manifest document: the live
    /// conversation window as turn IDs (pinned and injected turns excluded),
    /// plus the small state history cannot rebuild. See `context::manifest`.
    pub(crate) fn to_manifest_doc(&self) -> ManifestDoc {
        ManifestDoc {
            version: FORMAT_VERSION,
            conversation_turn_ids: self
                .turns
                .iter()
                .filter(|t| !t.is_pinned() && !self.injected_turn_ids.contains(&t.id.0))
                .map(|t| t.id.0)
                .collect(),
            cumulative_usage: self.cumulative_usage,
            next_turn_id: self.next_turn_id,
            retry_log: self.retry_log.clone(),
        }
    }

    /// Project the pinned layer onto the pins document, in composition order.
    /// Pinned turns are single-message by construction (`pin`/`replace_pin`
    /// store exactly one message).
    pub(crate) fn to_pins_doc(&self) -> PinsDoc {
        PinsDoc {
            version: FORMAT_VERSION,
            pins: self
                .pinned_turns()
                .map(|t| PinRecord {
                    id: t.id.0,
                    label: t.label().expect("pinned turn has a label").to_owned(),
                    message: t.messages[0].clone(),
                    estimated_tokens: t.estimated_tokens,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
