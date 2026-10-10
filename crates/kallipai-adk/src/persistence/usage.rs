//! Read-only projection of an agent's cumulative token usage.
//!
//! Task usage accounting snapshots every agent's lifetime counters when a
//! task enters or leaves an active window. Live agents answer from their
//! in-memory context store; everything else (idle bodies on disk, parked
//! bodies in the inactive area) answers from the manifest, which carries
//! the same counters the store last persisted.

use std::path::Path;

use kallipai_common::context::CumulativeUsage;

use super::io::{backup_path, load_with_backup};

/// Read the cumulative usage counters out of an agent directory.
///
/// Deliberately narrower than [`super::restore_agent`]: it parses only
/// `manifest.json` (falling back to its backup) and never hydrates history,
/// pins, or migrations, so a parked body can be read cheaply and without
/// side effects.
///
/// `None` means "not readable" — a missing directory, a missing manifest, or
/// damage in both the manifest and its backup. Callers must record a skip
/// rather than substituting zero: zero is a real count, and a silent zero
/// would understate what a task consumed.
pub fn read_cumulative_usage(dir: &Path) -> Option<CumulativeUsage> {
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.exists() && !backup_path(&manifest_path).exists() {
        return None;
    }
    match load_with_backup::<crate::context::manifest::ManifestDoc>(&manifest_path) {
        Ok((doc, _)) => Some(doc.cumulative_usage),
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                "cumulative usage unreadable, skipping this agent: {e:#}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests;
