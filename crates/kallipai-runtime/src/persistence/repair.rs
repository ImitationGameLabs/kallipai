use std::path::Path;

use anyhow::{Context as _, Result};

use super::store::Degradation;
use super::store::load_store;
use just_llm_client::types::generation::Message;

/// One turn's pairing damage and the fix applied to it.
#[derive(Debug, Clone)]
pub struct RepairAction {
    /// The damaged turn's ID — the repaired history record keeps it.
    pub turn_id: u64,
    /// Dangling tool-call ids that got a synthesized not-executed result.
    pub unanswered_calls: Vec<String>,
    /// Orphan tool-result ids whose messages were removed.
    pub orphan_results: Vec<String>,
}

/// Outcome of a [`repair_agent_context`] pass.
#[derive(Debug)]
pub struct RepairReport {
    /// Damage absorbed while loading (backup fallback, tail recovery, …):
    /// repair rides the normal degradation chain, so the report doubles as
    /// a health check.
    pub degraded: Vec<Degradation>,
    /// The repairs applied — or, in a dry run, the repairs that would be.
    pub actions: Vec<RepairAction>,
}

/// Repair tool-call/result pairing damage in an agent's persisted context.
///
/// Every conversation turn violating the pairing invariant — dangling calls
/// (answered with a synthesized not-executed error, the honest outcome for
/// a call that never ran) or orphan results (removed) — is fixed in memory,
/// and the repaired messages are appended to today's history file under the
/// same turn ID. Both history readers scan newest-first, so the appended
/// record wins over the damaged one, the log stays append-only, and the
/// manifest needs no write: turn IDs are unchanged, and split persistence
/// keeps message content only in history.
///
/// `execute: false` is a dry run — `actions` lists what would be done
/// and nothing is written, not even the deferred legacy→split migration,
/// which only a restore completes (see the comment at `load_store`).
///
/// Idempotent: a repaired record is itself clean, so a second pass finds
/// nothing to append.
pub fn repair_agent_context(
    agent_dir: &Path,
    tail_budget: usize,
    execute: bool,
) -> Result<RepairReport> {
    let mut degraded = Vec::new();
    // Repair never persists the store, so a legacy directory's deferred
    // migration stays pending for the next restore to finish.
    let (mut store, _migrate_pending) = load_store(agent_dir, tail_budget, &mut degraded)?;
    let history = crate::history::HistoryWriter::new(agent_dir.to_owned());
    let mut actions = Vec::new();

    for turn in store.turns_mut() {
        // Pinned turns carry no tool traffic (see `check_tool_pairing`).
        if turn.is_pinned() {
            continue;
        }
        let unanswered = crate::tool_execution::unanswered_call_ids(&turn.messages);
        let orphan = crate::tool_execution::orphan_result_ids(&turn.messages);
        if unanswered.is_empty() && orphan.is_empty() {
            continue;
        }
        actions.push(RepairAction {
            turn_id: turn.id.0,
            unanswered_calls: unanswered.iter().map(|(id, _)| id.clone()).collect(),
            orphan_results: orphan.clone(),
        });
        if !execute {
            continue;
        }

        // Synthesis mirrors `tool_execution::synthesize_unanswered_results`
        // minus the live-loop specifics: no park target is recorded for a
        // dangling `break`, so every dangling call — `break` included — gets
        // the honest not-executed error.
        for (id, name) in unanswered {
            let content = crate::policy::error_result(
                &name,
                "not executed: no result was ever recorded for this call (offline repair)"
                    .to_owned(),
            );
            turn.messages.push(Message::tool(content, id));
        }
        let orphan_ids: Vec<&str> = orphan.iter().map(String::as_str).collect();
        turn.messages.retain(|m| {
            m.tool_call_id()
                .is_none_or(|rid| !orphan_ids.contains(&rid))
        });

        turn.estimated_tokens = crate::context::Turn::estimate_tokens(&turn.messages);
        history
            .append(
                Some(turn.id.0),
                &turn.messages,
                turn.estimated_tokens,
                crate::history::RecordKind::Turn,
                None,
                &[],
            )
            .with_context(|| format!("appending repaired turn {} to history", turn.id.0))?;
    }

    Ok(RepairReport { degraded, actions })
}

#[cfg(test)]
mod tests;
