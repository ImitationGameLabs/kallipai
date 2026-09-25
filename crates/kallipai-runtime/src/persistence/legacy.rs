use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::io::write_with_backup;
use crate::context::ContextStore;

pub(crate) fn legacy_archive_path(dir: &Path) -> PathBuf {
    dir.join("context.legacy.json")
}

/// Remove prior restart notices from a legacy store before migration: they
/// are on-the-spot prompts, meaningless across restarts, and have no
/// history record to hydrate from. Matched by content against the current
/// message; wording drift in an older notice leaves the turn in place, where
/// the missing-ID degradation absorbs it.
pub(crate) fn strip_restart_turns(store: &mut ContextStore) {
    let restart_positions: Vec<usize> = store
        .turns()
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            !t.is_pinned()
                && t.messages.len() == 1
                && t.messages[0].content() == Some(RESTART_MESSAGE)
        })
        .map(|(i, _)| i)
        .collect();
    for i in restart_positions.into_iter().rev() {
        store.drain_turns(i..i + 1);
    }
}

/// Migrate a legacy store to split persistence, in an order that leaves no
/// losing intermediate state: pins first, manifest second, legacy rename
/// last. `manifest.json` present is the completed-migration marker, so a
/// crash after pins but before manifest re-runs this whole path from the
/// intact `context.json`; a crash after manifest but before the rename takes
/// the split path on restart and the dispatcher finishes the rename.
pub(crate) fn migrate_legacy_to_split(dir: &Path, store: &ContextStore) -> Result<()> {
    let pins = serde_json::to_string(&store.to_pins_doc()).context("serializing pins.json")?;
    let manifest =
        serde_json::to_string(&store.to_manifest_doc()).context("serializing manifest.json")?;
    write_with_backup(&dir.join("pins.json"), &pins)?;
    write_with_backup(&dir.join("manifest.json"), &manifest)?;
    fs::rename(dir.join("context.json"), legacy_archive_path(dir))?;
    Ok(())
}

pub(crate) const RESTART_MESSAGE: &str = concat!(
    "[system]\n",
    "Agent restored from a previous state. Shell sessions have been reset \u{2014}\n",
    "environment variables, working directory, and background processes are no\n",
    "longer available. Review the current state of the project and re-establish\n",
    "any necessary conditions before continuing. Directory write-locks are managed by\n",
    "the system for the lifetime of your task and were re-established on restore; they\n",
    "need no action from you.\n"
);

#[cfg(test)]
mod tests;
