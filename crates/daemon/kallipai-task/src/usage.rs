//! Token accounting over a task's life: the snapshot every window edge
//! carries, and the sum derived from those snapshots at read time.
//!
//! A *window* is `[baseline, terminal)`. Baselines are `start`, `resume`, and
//! `reopen`; terminals are `pause` and `close`. The state machine allows one
//! open window at a time, so a task's spend is the sum of its windows:
//! pausing and closing book the work done so far, and resuming or reopening
//! re-base the counters, which keeps time spent elsewhere in between out.
//!
//! Snapshots ride in the payload of the transition event they belong to, in
//! the same transaction that moves the machine, so there is no instant at
//! which the state moved and the numbers went missing.

use std::collections::{BTreeMap, BTreeSet};

use kallipai_common::context::CumulativeUsage;
use kallipai_common::protocol::{
    TaskUsageExport, UsageAgentExport, UsageGapExport, UsageGapReason, UsageSegmentExport,
};
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::entities::task_event;

/// Payload key under which a transition event carries its snapshot.
pub const USAGE_SNAPSHOT_KEY: &str = "usage_snapshot";

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// The agent's live context store: it was running at capture time.
    Live,
    /// The agent's `manifest.json`: an idle or parked body.
    Disk,
}

/// One agent's cumulative counters, with the provenance of the reading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedUsage {
    pub usage: CumulativeUsage,
    pub source: UsageSource,
}

/// An agent the capture could not read.
///
/// Never recorded as zero: zero is a real count, and a silent zero would
/// understate the window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedAgent {
    pub agent_id: String,
    pub reason: String,
}

/// Every agent's cumulative counters at one state move.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub agents: BTreeMap<String, CapturedUsage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedAgent>,
    /// When the capture ran (unix seconds) — not when the event was written.
    pub captured_at: i64,
    /// Set when the capture as a whole failed. The move proceeds regardless:
    /// accounting never blocks a state transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
}

/// The payload for a transition event that carries a snapshot. `None` in,
/// `None` out: a move with nothing captured writes no payload, which keeps
/// the trail of a pre-feature task byte-identical to what it was.
pub fn transition_payload(
    snapshot: Option<&UsageSnapshot>,
) -> Result<Option<String>, serde_json::Error> {
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let mut payload = serde_json::Map::new();
    payload.insert(
        USAGE_SNAPSHOT_KEY.to_string(),
        serde_json::to_value(snapshot)?,
    );
    Ok(Some(serde_json::Value::Object(payload).to_string()))
}

/// Adds the snapshot to an already-built payload: `close` carries the reason,
/// the summary, and the archive pointer alongside it.
pub fn insert_snapshot(
    payload: &mut serde_json::Value,
    snapshot: Option<&UsageSnapshot>,
) -> Result<(), serde_json::Error> {
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    let object = payload.as_object_mut().ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "transition payload is not a JSON object",
        ))
    })?;
    object.insert(
        USAGE_SNAPSHOT_KEY.to_string(),
        serde_json::to_value(snapshot)?,
    );
    Ok(())
}

/// One open window, held between its baseline event and its terminal.
struct Window {
    from_event: String,
    from_at: i64,
    /// Counters as read at the baseline.
    agents: BTreeMap<String, CumulativeUsage>,
    /// Agents the baseline capture could not read.
    skipped: BTreeSet<String>,
    /// The baseline's whole-capture failure, if any.
    capture_error: bool,
}

/// What a transition event's payload says about its snapshot.
enum Readout {
    /// No snapshot key: the move recorded nothing (a pre-feature task, or a
    /// transition that is not a window edge).
    Absent,
    Present(Box<UsageSnapshot>),
    /// The key is there but does not parse: a data gap, not a hard error.
    Unreadable,
}

fn readout(payload: Option<&str>) -> Readout {
    let Some(raw) = payload else {
        return Readout::Absent;
    };
    // A payload that is not JSON at all belongs to some other feature; the
    // export path already surfaces that as a corrupt record. Only our own
    // key, present and unparsable, is a usage gap.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Readout::Absent;
    };
    let Some(snapshot) = value.get(USAGE_SNAPSHOT_KEY) else {
        return Readout::Absent;
    };
    match serde_json::from_value::<UsageSnapshot>(snapshot.clone()) {
        Ok(snapshot) => Readout::Present(Box::new(snapshot)),
        Err(_) => Readout::Unreadable,
    }
}

/// Terminal minus baseline, clamped at zero per counter. The bool reports a
/// counter that went backwards (a repaired or reset body), which the caller
/// records as `non_monotonic`; the raw values stay in the payload.
fn subtract(terminal: &CumulativeUsage, baseline: &CumulativeUsage) -> (CumulativeUsage, bool) {
    fn delta(terminal: u64, baseline: u64) -> (u64, bool) {
        match terminal.checked_sub(baseline) {
            Some(value) => (value, false),
            None => (0, true),
        }
    }
    let (prompt_tokens, a) = delta(terminal.prompt_tokens, baseline.prompt_tokens);
    let (completion_tokens, b) = delta(terminal.completion_tokens, baseline.completion_tokens);
    let (cache_read_tokens, c) = delta(terminal.cache_read_tokens, baseline.cache_read_tokens);
    let (cache_write_tokens, d) = delta(terminal.cache_write_tokens, baseline.cache_write_tokens);
    (
        CumulativeUsage {
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_write_tokens,
        },
        a || b || c || d,
    )
}

fn accumulate(total: &mut CumulativeUsage, delta: &CumulativeUsage) {
    total.prompt_tokens += delta.prompt_tokens;
    total.completion_tokens += delta.completion_tokens;
    total.cache_read_tokens += delta.cache_read_tokens;
    total.cache_write_tokens += delta.cache_write_tokens;
}

fn row(agent_id: &str, usage: &CumulativeUsage) -> UsageAgentExport {
    UsageAgentExport {
        agent_id: agent_id.to_string(),
        agent_role: None,
        consumed_tokens: usage.consumed(),
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
    }
}

fn gap(agent_id: &str, segment: usize, reason: UsageGapReason) -> UsageGapExport {
    UsageGapExport {
        agent_id: agent_id.to_string(),
        segment,
        reason,
    }
}

/// Sums a trail into a task's token accounting.
///
/// `events` must be in trail order (ascending id): windows pair on it.
/// `Ok(None)` means the task predates the feature — no transition carries a
/// snapshot, so there is nothing to report, which is not the same as zero.
///
/// The total covers the task's **active windows** only. Work the same agent
/// did elsewhere while a window was open (another task, an unrelated turn)
/// still lands in these numbers; the export's prose carries that caveat, and
/// it is deliberately not a `partial` marker: it is the shape of the
/// measurement, not a hole in the data.
pub fn aggregate(events: &[task_event::Model]) -> Result<Option<TaskUsageExport>, Error> {
    let mut seen_snapshot = false;
    let mut open: Option<Window> = None;
    let mut segments: Vec<UsageSegmentExport> = Vec::new();
    let mut totals: BTreeMap<String, CumulativeUsage> = BTreeMap::new();
    let mut gaps: Vec<UsageGapExport> = Vec::new();
    let mut non_monotonic: BTreeSet<String> = BTreeSet::new();
    let mut partial = false;
    // 1-based ordinal of the window being matched, counted in trail order.
    // A terminal with no baseline consumes one too, so a gap always names a
    // window the trail actually reached.
    let mut ordinal: usize = 0;
    // A window whose baseline edge carries no snapshot: its spend cannot
    // be measured between edges, so the task reads as incomplete.
    let mut bare_window = false;

    for event in events {
        if event.kind != "transition" {
            continue;
        }
        if !matches!(
            event.name.as_str(),
            "start" | "resume" | "reopen" | "pause" | "close"
        ) {
            continue;
        }
        let (snapshot, unreadable) = match readout(event.payload.as_deref()) {
            Readout::Absent => (None, false),
            Readout::Present(snapshot) => (Some(*snapshot), false),
            Readout::Unreadable => (None, true),
        };
        if snapshot.is_some() || unreadable {
            seen_snapshot = true;
        }
        if unreadable {
            partial = true;
        }

        match (snapshot, event.name.as_str()) {
            (snapshot, "start" | "resume" | "reopen") => {
                if let Some(abandoned) = open.take() {
                    // The machine never opens a window on top of an open one;
                    // a trail that does is inconsistent, so book the abandoned
                    // window's agents instead of dropping them.
                    partial = true;
                    ordinal += 1;
                    for agent_id in abandoned.agents.keys().chain(abandoned.skipped.iter()) {
                        let entry = gap(agent_id, ordinal, UsageGapReason::MissingTerminal);
                        gaps.push(entry);
                    }
                }
                match snapshot {
                    Some(snapshot) => {
                        open = Some(Window {
                            from_event: event.name.clone(),
                            from_at: event.created_at,
                            agents: snapshot
                                .agents
                                .iter()
                                .map(|(id, captured)| (id.clone(), captured.usage))
                                .collect(),
                            skipped: snapshot
                                .skipped
                                .iter()
                                .map(|skipped| skipped.agent_id.clone())
                                .collect(),
                            capture_error: snapshot.capture_error.is_some(),
                        });
                    }
                    None => bare_window = true,
                }
            }
            (snapshot, _) => {
                ordinal += 1;
                let gaps_before = gaps.len();
                match (open.take(), snapshot) {
                    (Some(window), Some(snapshot)) => {
                        let mut rows = Vec::new();
                        let mut segment_total = 0u64;
                        let mut ids: BTreeSet<String> = window.agents.keys().cloned().collect();
                        ids.extend(snapshot.agents.keys().cloned());
                        for agent_id in ids {
                            match (window.agents.get(&agent_id), snapshot.agents.get(&agent_id)) {
                                (Some(baseline), Some(terminal)) => {
                                    let (delta, backwards) = subtract(&terminal.usage, baseline);
                                    if backwards {
                                        non_monotonic.insert(agent_id.clone());
                                    }
                                    segment_total += delta.consumed();
                                    accumulate(totals.entry(agent_id.clone()).or_default(), &delta);
                                    rows.push(row(&agent_id, &delta));
                                }
                                (Some(_), None) => {
                                    let entry =
                                        gap(&agent_id, ordinal, UsageGapReason::MissingTerminal);
                                    gaps.push(entry);
                                }
                                (None, Some(_)) => {
                                    let entry =
                                        gap(&agent_id, ordinal, UsageGapReason::MissingBaseline);
                                    gaps.push(entry);
                                }
                                (None, None) => {}
                            }
                        }
                        // An unreadable capture leaves the agent out of both
                        // maps, so it would otherwise vanish silently.
                        let mut booked: BTreeSet<String> = gaps[gaps_before..]
                            .iter()
                            .map(|g| g.agent_id.clone())
                            .collect();
                        let terminal_skips = snapshot.skipped.iter().map(|s| &s.agent_id);
                        for agent_id in window.skipped.iter().chain(terminal_skips) {
                            if snapshot.agents.contains_key(agent_id)
                                || window.agents.contains_key(agent_id)
                                || !booked.insert(agent_id.clone())
                            {
                                continue;
                            }
                            let reason = if window.skipped.contains(agent_id) {
                                UsageGapReason::MissingBaseline
                            } else {
                                UsageGapReason::MissingTerminal
                            };
                            let entry = gap(agent_id, ordinal, reason);
                            gaps.push(entry);
                        }
                        if window.capture_error || snapshot.capture_error.is_some() {
                            partial = true;
                        }
                        segments.push(UsageSegmentExport {
                            from_event: window.from_event,
                            to_event: event.name.clone(),
                            from_at: crate::model::iso8601_utc(window.from_at)?,
                            to_at: crate::model::iso8601_utc(event.created_at)?,
                            total_tokens: segment_total,
                            agents: rows,
                        });
                    }
                    (Some(window), None) => {
                        // The window closed without a readable terminal, so
                        // its agents are unaccounted for.
                        partial = true;
                        let mut ids: BTreeSet<String> = window.agents.keys().cloned().collect();
                        ids.extend(window.skipped.iter().cloned());
                        for agent_id in ids {
                            let entry = gap(&agent_id, ordinal, UsageGapReason::MissingTerminal);
                            gaps.push(entry);
                        }
                    }
                    (None, Some(snapshot)) => {
                        // A terminal whose baseline never carried a snapshot:
                        // every reading in it lacks the value to subtract.
                        partial = true;
                        let mut ids: BTreeSet<String> = snapshot.agents.keys().cloned().collect();
                        ids.extend(snapshot.skipped.iter().map(|s| s.agent_id.clone()));
                        for agent_id in ids {
                            let entry = gap(&agent_id, ordinal, UsageGapReason::MissingBaseline);
                            gaps.push(entry);
                        }
                    }
                    (None, None) => bare_window = true,
                }
                if gaps.len() > gaps_before {
                    partial = true;
                }
            }
        }
    }

    if !seen_snapshot {
        return Ok(None);
    }
    if bare_window {
        // A window whose baseline edge recorded nothing, while other
        // windows carry snapshots: the numbers are incomplete.
        partial = true;
    }
    let open_since = match &open {
        Some(window) => {
            // An open window contributes no segment: its spend is unknown
            // until its terminal arrives, and `open` plus `open_since` mark
            // where it started. A baseline the capture could not read in
            // full (a failed scan, or agents it had to skip) leaves the
            // task's numbers incomplete.
            if window.capture_error || !window.skipped.is_empty() {
                partial = true;
            }
            Some(crate::model::iso8601_utc(window.from_at)?)
        }
        None => None,
    };
    let total_tokens = segments.iter().map(|segment| segment.total_tokens).sum();
    let agents = totals
        .iter()
        .map(|(agent_id, usage)| row(agent_id, usage))
        .collect();
    Ok(Some(TaskUsageExport {
        total_tokens,
        open: open.is_some(),
        open_since,
        segments,
        agents,
        gaps,
        partial,
        non_monotonic: non_monotonic.into_iter().collect(),
    }))
}

#[cfg(test)]
mod tests;
