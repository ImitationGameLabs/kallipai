//! Snapshot every agent's cumulative token counters at a task state move.
//!
//! The task ledger stores what this module captures, in the payload of the
//! transition event it belongs to; the read face sums the deltas between a
//! window's two edges. The capture is best-effort by design — a task move
//! must never fail because an accountant could not read a counter: an
//! unreadable agent is recorded as a skip and a failed scan as a
//! `capture_error`, and both surface as `partial` on the read face instead
//! of as an error here.
//!
//! The agent set is the union of three sources, deduplicated by id with the
//! live reading winning: the live registry, the agent directories eligible
//! for restore, and the parked bodies in the inactive area.
//!
//! The capture runs before the verb's gate checks, so a refused move pays
//! it too: the trade is deliberate (the snapshot rides in the same
//! transaction as the state move, which no post-gate placement can give).
//! It is bounded by [`CAPTURE_BUDGET`] for the same reason a move must not
//! wait on a slow body. The budget spans the awaits a capture takes (the
//! registry read, a contended store lock); it cannot preempt a synchronous
//! filesystem call once it is inside the kernel, which is where a slow
//! scan sits.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kallipai_adk::context::ContextStore;
use kallipai_adk::persistence;
use kallipai_task::usage::{CapturedUsage, SkippedAgent, UsageSnapshot, UsageSource};
use tokio::sync::Mutex;

use crate::state::AppState;

/// Where an agent's counters will be read from. Collected while the registry
/// read lock is held; the readings themselves happen after it is released.
enum Source {
    /// A running agent answers from its live context store.
    Live(Arc<Mutex<ContextStore>>),
    /// Anything else answers from its directory on disk.
    Dir(PathBuf),
}

/// How long a capture may run before the move proceeds unaccounted.
///
/// A healthy capture reads a handful of small JSON files — milliseconds.
/// The budget is three orders of magnitude above that, and an elapse is
/// not silent: it lands in the same `capture_error` slot as a failed scan,
/// which the read face reports as `partial`.
const CAPTURE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads every agent's cumulative counters into one snapshot, bounded by
/// [`CAPTURE_BUDGET`].
pub async fn capture_usage(state: &AppState) -> UsageSnapshot {
    match tokio::time::timeout(CAPTURE_BUDGET, capture_usage_inner(state)).await {
        Ok(snapshot) => snapshot,
        Err(_) => UsageSnapshot {
            agents: BTreeMap::new(),
            skipped: Vec::new(),
            captured_at: now_secs(),
            capture_error: Some(format!("capture exceeded {}s", CAPTURE_BUDGET.as_secs())),
        },
    }
}

/// The capture itself, unbounded: the public entry point wraps it.
async fn capture_usage_inner(state: &AppState) -> UsageSnapshot {
    let captured_at = now_secs();
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    let mut skipped: BTreeMap<String, String> = BTreeMap::new();
    let mut capture_error: Option<String> = None;

    {
        let registry = state.registry.read().await;
        for (id, entry) in registry.iter() {
            let agent_id = id.to_string();
            match entry.as_live() {
                Some(live) => {
                    sources.insert(agent_id, Source::Live(live.agent.store.clone()));
                }
                None => {
                    if let Some(dir) = entry.identity().agent_dir.clone() {
                        sources.insert(agent_id, Source::Dir(dir));
                    } else {
                        skipped.insert(agent_id, "no agent directory".to_string());
                    }
                }
            }
        }
    }

    // Bodies the registry does not hold: directories eligible for restore,
    // and parked bodies in the inactive area (a deactivated agent's counters
    // are still on disk, and the task it worked on still owns them).
    match persistence::scan_agents() {
        Ok((pending, refused)) => {
            for entry in pending {
                sources
                    .entry(entry.agent_id.to_string())
                    .or_insert(Source::Dir(entry.agent_dir));
            }
            for entry in refused {
                sources
                    .entry(entry.agent_id.to_string())
                    .or_insert(Source::Dir(entry.agent_dir));
            }
        }
        Err(e) => capture_error = Some(format!("agent scan failed: {e:#}")),
    }
    match persistence::scan_inactive() {
        Ok(parked) => {
            for (id, _meta) in parked {
                let agent_id = id.to_string();
                if sources.contains_key(&agent_id) {
                    continue;
                }
                match persistence::inactive_dir(&id) {
                    Ok(dir) => {
                        sources.insert(agent_id, Source::Dir(dir));
                    }
                    Err(e) => {
                        skipped.insert(agent_id, format!("inactive directory: {e:#}"));
                    }
                }
            }
        }
        Err(e) => {
            capture_error = Some(match capture_error {
                Some(previous) => format!("{previous}; inactive scan failed: {e:#}"),
                None => format!("inactive scan failed: {e:#}"),
            })
        }
    }

    let mut agents = BTreeMap::new();
    for (agent_id, source) in sources {
        match source {
            Source::Live(store) => {
                let guard = store.lock().await;
                let usage = *guard.cumulative_usage();
                drop(guard);
                agents.insert(
                    agent_id,
                    CapturedUsage {
                        usage,
                        source: UsageSource::Live,
                    },
                );
            }
            Source::Dir(dir) => match persistence::read_cumulative_usage(&dir) {
                Some(usage) => {
                    agents.insert(
                        agent_id,
                        CapturedUsage {
                            usage,
                            source: UsageSource::Disk,
                        },
                    );
                }
                None => {
                    skipped.insert(
                        agent_id,
                        format!("no readable manifest in {}", dir.display()),
                    );
                }
            },
        }
    }

    UsageSnapshot {
        agents,
        skipped: skipped
            .into_iter()
            .map(|(agent_id, reason)| SkippedAgent { agent_id, reason })
            .collect(),
        captured_at,
        capture_error,
    }
}

fn now_secs() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
