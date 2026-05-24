//! Single source of truth for all context data in an agent session.

use std::collections::VecDeque;
use std::ops::Range;

use anyhow::{Result, bail};
use just_llm_client::types::chat::{ChatMessage, ToolDefinition};

use super::turn::{Turn, TurnId, estimate_message_tokens};

/// A pinned context item with a label for identification and lifecycle.
#[derive(Clone, Debug)]
pub struct PinnedItem {
    pub label: String,
    pub message: ChatMessage,
}

/// Snapshot of current context layer breakdown and last known token usage.
///
/// `last_prompt_tokens` comes from the provider's response `usage` field —
/// the most accurate token count available. Layer breakdowns use heuristic
/// estimates for informational purposes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ContextUsage {
    /// Per-item breakdown: (label, estimated_tokens).
    pub pinned_items: Vec<(String, usize)>,
    /// Estimated tokens for the summary layer.
    pub summary_tokens: usize,
    /// Number of stored conversation turns.
    pub turn_count: usize,
    /// Estimated tokens across all turns.
    pub turn_tokens: usize,
    /// Exact prompt token count from the last provider response, if any.
    pub last_prompt_tokens: Option<u32>,
}

impl ContextUsage {
    pub fn format_summary(&self) -> String {
        let pinned_tokens: usize = self.pinned_items.iter().map(|(_, t)| *t).sum();
        format!(
            "turns: {} ({} est tokens), pinned: {} ({} tokens), summary: {} tokens, last prompt: {}",
            self.turn_count,
            self.turn_tokens,
            self.pinned_items.len(),
            pinned_tokens,
            self.summary_tokens,
            self.last_prompt_tokens
                .map(|t| t.to_string())
                .unwrap_or_else(|| "n/a".into()),
        )
    }
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
    /// Return the labels of all currently pinned items.
    fn pinned_labels(&self) -> Vec<String>;
    /// Return a snapshot of current context layer breakdown.
    fn usage_snapshot(&self) -> ContextUsage;
    /// Evict the oldest `count` turns. Returns actual count evicted.
    fn evict_turns(&mut self, count: usize) -> EvictResult;
}

/// Single source of truth for all context data in an agent session.
///
/// Owns tool definitions, pinned messages, conversation turns,
/// and an optional summary of compacted history.
/// Budget checking is handled by the main loop using ChatClient's
/// accurate token estimation pipeline.
pub struct ContextStore {
    /// Tool definitions: part of every request.
    tool_definitions: Vec<ToolDefinition>,
    /// Pinned items: always included, never subject to compaction.
    pinned: Vec<PinnedItem>,
    /// Logical conversation turns in chronological order (newest at back).
    turns: VecDeque<Turn>,
    /// Accumulated summary of compacted turns, if any.
    summary: Option<String>,
    /// Estimated token count for the current summary.
    summary_tokens: usize,
    /// Exact prompt token count from the last provider response.
    last_prompt_tokens: Option<u32>,
    /// The next turn ID to assign.
    next_turn_id: u64,
}

impl AgenticContext for ContextStore {
    fn pin(&mut self, label: &str, message: ChatMessage) -> Result<()> {
        if self.pinned.iter().any(|p| p.label == label) {
            bail!("pinned item '{label}' already exists");
        }
        self.pinned
            .push(PinnedItem { label: label.to_owned(), message });
        Ok(())
    }

    fn unpin(&mut self, label: &str) -> Result<()> {
        let idx = self
            .pinned
            .iter()
            .position(|p| p.label == label)
            .ok_or_else(|| anyhow::anyhow!("pinned item '{label}' not found"))?;
        self.pinned.remove(idx);
        Ok(())
    }

    fn pinned_labels(&self) -> Vec<String> {
        self.pinned.iter().map(|p| p.label.clone()).collect()
    }

    fn usage_snapshot(&self) -> ContextUsage {
        let pinned_items: Vec<(String, usize)> = self
            .pinned
            .iter()
            .map(|p| (p.label.clone(), estimate_message_tokens(&p.message)))
            .collect();
        let turn_tokens: usize = self.turns.iter().map(|t| t.estimated_tokens).sum();
        ContextUsage {
            pinned_items,
            summary_tokens: self.summary_tokens,
            turn_count: self.turns.len(),
            turn_tokens,
            last_prompt_tokens: self.last_prompt_tokens,
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
        EvictResult { evicted: to_evict, remaining_turns: self.turns.len(), freed_tokens }
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
            next_turn_id: 0,
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

    /// Cache the exact prompt token count from a provider response.
    pub fn set_last_usage(&mut self, prompt_tokens: u32) {
        self.last_prompt_tokens = Some(prompt_tokens);
    }

    /// Append a new turn from the given messages.
    /// Returns the assigned turn ID.
    pub fn push_turn(&mut self, messages: Vec<ChatMessage>) -> TurnId {
        let estimated_tokens = Turn::estimate_tokens(&messages);
        let id = TurnId(self.next_turn_id);
        self.next_turn_id += 1;
        self.turns
            .push_back(Turn { id, messages, estimated_tokens });
        id
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

    /// The current summary, if any.
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Replace the summary (called by compaction).
    pub fn set_summary(&mut self, text: String) {
        self.summary_tokens = text.chars().count() / 4 + 16;
        self.summary = Some(text);
    }

    /// Remove turns in the given range and return them.
    pub fn drain_turns(&mut self, range: Range<usize>) -> Vec<Turn> {
        self.turns.drain(range).collect()
    }

    /// Re-insert turns at the front of the queue.
    pub fn prepend_turns(&mut self, turns: Vec<Turn>) {
        for turn in turns.into_iter().rev() {
            self.turns.push_front(turn);
        }
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
        let id0 = store.push_turn(vec![ChatMessage::user("a")]);
        let id1 = store.push_turn(vec![ChatMessage::user("b")]);
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
    fn set_summary_updates_tokens() {
        let mut store = new_store();
        store.set_summary("a summary".into());
        assert_eq!(store.summary(), Some("a summary"));
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
}
