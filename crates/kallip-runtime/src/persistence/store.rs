use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};

use super::io::backup_path;
use super::io::load_with_backup;
use super::legacy::{legacy_archive_path, strip_restart_turns};
use crate::context::ContextStore;
use just_llm_client::types::generation::Message;

/// One non-fatal damage event absorbed during restore.
#[derive(Debug, Clone)]
pub struct Degradation {
    pub kind: DegradationKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationKind {
    /// History records missing for manifest-referenced turn IDs.
    MissingHistoryTurns,
    /// History lines that failed to parse and were skipped.
    BadHistoryLines,
    /// Tool calls in a restored turn with no matching tool result.
    UnansweredToolCalls,
    /// Tool results in a restored turn answering no call in that turn.
    OrphanToolResults,
    /// manifest.json and its backup both unreadable or missing; window
    /// rebuilt from the history tail within a capped budget, leading
    /// damaged turns dropped.
    TailRecovery,
    /// pins.json and its backup both unreadable or missing; booted with
    /// empty pins.
    PinsLost,
}

/// Load the context store in the format the directory holds.
///
/// `manifest.json` (or its backup) present → split format: parse both
/// documents and hydrate the conversation window from history by turn
/// ID. A leftover `context.json` (migration interrupted before the
/// rename) is finished off. Only `context.json` → legacy format: load
/// it and strip restart notices; the split documents are written by
/// the caller after its transformations (the flag in the return says
/// when). Neither split document but history exists → the window is
/// rebuilt from the history tail. Nothing at all → fresh store.
pub(crate) fn load_store(
    dir: &Path,
    tail_budget: usize,
    degraded: &mut Vec<Degradation>,
) -> Result<(ContextStore, bool)> {
    let manifest_path = dir.join("manifest.json");
    let legacy_path = dir.join("context.json");

    if manifest_path.exists() || backup_path(&manifest_path).exists() {
        if legacy_path.exists()
            && let Err(e) = fs::rename(&legacy_path, legacy_archive_path(dir))
        {
            tracing::warn!("finishing legacy rename failed: {e:#}");
        }

        let manifest =
            match load_with_backup::<crate::context::manifest::ManifestDoc>(&manifest_path) {
                Ok((doc, false)) => doc,
                Ok((doc, true)) => {
                    tracing::warn!("manifest.json unreadable or missing, recovered from backup");
                    doc
                }
                // Both wrecked: rebuild the window from the history tail instead
                // of faulting the agent (the manifest alone is not worth a
                // Faulted — history and pins survive independently).
                Err(e) => {
                    return Ok((
                        rebuild_window_from_tail(dir, tail_budget, degraded, e),
                        false,
                    ));
                }
            };
        let pins = load_pins(dir, degraded);

        let (convo, report) = crate::history::hydrate_turns(dir, &manifest.conversation_turn_ids);
        if report.bad_lines > 0 {
            degraded.push(Degradation {
                kind: DegradationKind::BadHistoryLines,
                detail: format!(
                    "{} history lines failed to parse and were skipped",
                    report.bad_lines
                ),
            });
        }
        if !report.missing_ids.is_empty() {
            degraded.push(Degradation {
                kind: DegradationKind::MissingHistoryTurns,
                detail: format!("no history record for turns {:?}", report.missing_ids),
            });
        }
        return Ok((ContextStore::from_persisted(&pins, convo, &manifest), false));
    }

    if legacy_path.exists() {
        let mut store: ContextStore = serde_json::from_str(
            &fs::read_to_string(&legacy_path).context("reading context.json")?,
        )
        .context("parsing context.json")?;
        strip_restart_turns(&mut store);
        // Deferred migration: the caller writes the split documents after
        // its folds; writing here would persist the raw legacy shape.
        return Ok((store, true));
    }

    // Neither split document: with no history this is a fresh directory;
    // with history the manifest went missing, and a fresh store would
    // renumber turn IDs from zero, aliasing every history record.
    if crate::history::has_history(dir) {
        return Ok((
            rebuild_window_from_tail(
                dir,
                tail_budget,
                degraded,
                anyhow::anyhow!("manifest.json and its backup are missing"),
            ),
            false,
        ));
    }
    Ok((ContextStore::new(), false))
}

/// Load pins.json through its backup chain, degrading to empty pins
/// (recorded) when both are unreadable. Pins are re-creatable by re-pinning,
/// so a wrecked pins file boots the agent rather than faulting it.
fn load_pins(dir: &Path, degraded: &mut Vec<Degradation>) -> crate::context::manifest::PinsDoc {
    match load_with_backup::<crate::context::manifest::PinsDoc>(&dir.join("pins.json")) {
        Ok((doc, false)) => doc,
        Ok((doc, true)) => {
            tracing::warn!("pins.json unreadable or missing, recovered from backup");
            doc
        }
        Err(e) => {
            degraded.push(Degradation {
                kind: DegradationKind::PinsLost,
                detail: format!(
                    "pins.json and its backup are unreadable or missing ({e:#}); booted with empty pins"
                ),
            });
            crate::context::manifest::PinsDoc {
                version: crate::context::manifest::FORMAT_VERSION,
                pins: Vec::new(),
            }
        }
    }
}

/// Last-resort window rebuild when manifest.json and its backup are both
/// unreadable: pins keep their own file (and backup chain), the conversation
/// window is rehydrated from the newest history turns that fit `tail_budget`,
/// and `next_turn_id` is rescanned from history (`max + 1`) — reusing an ID
/// would silently alias history records and manifest references with
/// different content. Leading turns with pairing damage are dropped up to
/// the first clean boundary: a dangling call or orphan result at the window
/// head is a guaranteed provider rejection on the next round. Usage counters
/// and the retry log lived only in the manifest and are lost (reset to zero,
/// recorded).
fn rebuild_window_from_tail(
    dir: &Path,
    tail_budget: usize,
    degraded: &mut Vec<Degradation>,
    manifest_err: anyhow::Error,
) -> ContextStore {
    let pins = load_pins(dir, degraded);
    let mut turns = crate::history::tail_turns_within_budget(dir, tail_budget);
    let tail_len = turns.len();
    let damaged_leading = turns
        .iter()
        .take_while(|t| pairing_damaged(&t.messages))
        .count();
    turns.drain(..damaged_leading);
    let next_turn_id = crate::history::max_turn_id(dir) + 1;
    degraded.push(Degradation {
        kind: DegradationKind::TailRecovery,
        detail: format!(
            "manifest.json and its backup are unreadable or missing ({manifest_err:#}); rebuilt the window from the history tail: kept {} of {tail_len} turns within a {tail_budget}-token budget (dropped {damaged_leading} pairing-damaged leading turns); next_turn_id rescanned to {next_turn_id}; cumulative usage and retry log lost (reset)",
            turns.len()
        ),
    });
    let manifest = crate::context::manifest::ManifestDoc {
        version: crate::context::manifest::FORMAT_VERSION,
        conversation_turn_ids: turns.iter().map(|t| t.id.0).collect(),
        cumulative_usage: Default::default(),
        next_turn_id,
        retry_log: Vec::new(),
    };
    ContextStore::from_persisted(&pins, turns, &manifest)
}

/// Whether a turn's messages violate tool-call/result pairing in either
/// direction (`tool_execution::unanswered_call_ids` and its mirror).
fn pairing_damaged(messages: &[Message]) -> bool {
    !crate::tool_execution::unanswered_call_ids(messages).is_empty()
        || !crate::tool_execution::orphan_result_ids(messages).is_empty()
}

#[cfg(test)]
mod tests;
