//! WorkSchedule scheduling engine and phase executor.
//!
//! A tokio background task that wakes on store mutations (via the store's
//! [`Notify`]) and on a coarse wall-clock tick whose only job is comparing
//! the cached next deadline against `now`, recomputing phase transitions
//! from cron expressions and firing four-phase lifecycle actions:
//!
//! - **Start**: set on-duty, flush inbox, send wake prompt.
//! - **PreWarn** (T − `pre_warn_minutes`): notify agent that shift ends soon.
//! - **FinalWarn** (T − `final_warn_minutes`): tell agent to save work now.
//! - **End**: set off-duty, interrupt the current round.
//!
//! The engine recomputes its state from scratch on restart. If the current
//! time falls inside a work window, duty is set on-duty immediately; if
//! outside, off-duty. Missed warnings are never replayed: a restart
//! relocates the phase past them, and an engine that stalls past a window
//! end (long suspend) skips the warn side-effects when catching up — only
//! End still fires.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::duty::DutyStatus;
use crate::state::{AgentId, SharedState};
use crate::work_schedule::spec::Spec;
use crate::work_schedule::{WorkSchedule, WorkScheduleStatus};

/// Tick cadence: how often the engine re-reads the wall clock and compares
/// it against the cached next-deadline. The tick never touches the DB — a
/// recompute (the only DB read) runs at cold start, on a store mutation,
/// or when a deadline is due. Every due/undue decision reads the wall
/// clock fresh, so nothing is quantized onto the monotonic clock and
/// system suspend / NTP steps cannot desynchronize the engine (the tick
/// itself pausing through a suspend is fine: the first tick after resume
/// sees the now-overdue deadline and catches up).
///
/// Transition latency is bounded by one tick — a deadline that becomes
/// due mid-tick fires on the next tick — which the work-schedule domain
/// tolerates. All schedule writes must flow through the store's notifying
/// mutators (create/update/delete) — the engine itself only reads the
/// store, and the tick does not poll, so an external write that bypasses
/// the mutators never notifies and stays invisible until the next due
/// recompute.
const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// How soon a failed recompute pass (transient DB error) becomes due
/// again. The retry itself is still tick-bound: this anchor merely makes
/// the deadline due so the next tick re-runs the pass.
const RETRY_BACKOFF_SECS: i64 = 10;

/// Pure form of the tick arm's due comparison, so the boundary is
/// unit-testable (the arm itself reads the wall clock and cannot be driven
/// without clock control). Inclusive: a deadline exactly at `now` is due.
fn deadline_due(deadline: OffsetDateTime, now: OffsetDateTime) -> bool {
    now >= deadline
}

/// Pure form of the failed-pass retry anchor ([`RETRY_BACKOFF_SECS`]).
fn retry_anchor(now: OffsetDateTime) -> OffsetDateTime {
    now + time::Duration::seconds(RETRY_BACKOFF_SECS)
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Lifecycle phase within a single work cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchedulePhase {
    /// Waiting for the next shift to start.
    OffDuty,
    /// On-duty, before the pre-warn threshold.
    Working,
    /// Pre-warn has been delivered.
    PreWarned,
    /// Final-warn has been delivered.
    FinalWarned,
}

/// Per-schedule runtime state, held in the engine task's local map.
#[derive(Clone)]
struct CycleState {
    /// Selected fields from the schedule, snapshotted to detect edits.
    spec: crate::work_schedule::spec::Spec,
    pre_warn_minutes: i64,
    final_warn_minutes: i64,
    final_warn_prompt: Option<String>,
    agent_id: AgentId,
    wake_prompt: String,
    /// Current phase in the cycle.
    phase: SchedulePhase,
    /// The end-time of the current work window (meaningful in Working..FinalWarned).
    end_time: OffsetDateTime,
    /// The next start-time (meaningful in OffDuty).
    next_start: OffsetDateTime,
}

/// A pending transition returned by [`compute_transition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transition {
    Start,
    PreWarn,
    FinalWarn,
    End,
}

// ---------------------------------------------------------------------------
// Pure scheduling logic
// ---------------------------------------------------------------------------

/// Determine whether `now` falls inside a work window, straight from the
/// spec.
///
/// Returns `(is_inside_window, next_start, next_end)`.
fn window_status(
    spec: &crate::work_schedule::spec::Spec,
    now: OffsetDateTime,
) -> Option<(bool, OffsetDateTime, OffsetDateTime)> {
    let st = crate::work_schedule::eval::window_status(spec, now)?;
    Some((st.inside, st.next_start, st.next_end))
}

/// Initialise a [`CycleState`] for a freshly-loaded schedule.
///
/// Sets duty immediately (recovery) and returns the state with the correct
/// starting phase. Caller is responsible for the side-effecting duty.set call;
/// this function only computes the state.
fn init_cycle(schedule: &WorkSchedule, root_id: &AgentId, now: OffsetDateTime) -> Option<CycleState> {
    let (inside, next_start, next_end) = window_status(&schedule.spec, now)?;

    let pre = schedule.pre_warn_minutes as i64;
    let fin = schedule.final_warn_minutes as i64;

    if inside {
        // Inside a work window — Start already happened. Determine how far
        // through the warning sequence we are based on time remaining.
        let pre_warn_time = next_end - time::Duration::minutes(pre);
        let final_warn_time = next_end - time::Duration::minutes(fin);

        let phase = if matches!(schedule.spec, Spec::Always) {
            // The always horizon has no warn semantics: it is a
            // wake-loop detail, not a shift end.
            SchedulePhase::Working
        } else if now >= final_warn_time {
            SchedulePhase::FinalWarned
        } else if now >= pre_warn_time {
            SchedulePhase::PreWarned
        } else {
            SchedulePhase::Working
        };

        Some(CycleState {
            spec: schedule.spec.clone(),
            pre_warn_minutes: pre,
            final_warn_minutes: fin,
            final_warn_prompt: schedule.final_warn_prompt.clone(),
            agent_id: root_id.clone(),
            wake_prompt: schedule.wake_prompt.clone(),
            phase,
            end_time: next_end,
            next_start,
        })
    } else {
        // Outside — waiting for the next start.
        Some(CycleState {
            spec: schedule.spec.clone(),
            pre_warn_minutes: pre,
            final_warn_minutes: fin,
            final_warn_prompt: schedule.final_warn_prompt.clone(),
            agent_id: root_id.clone(),
            wake_prompt: schedule.wake_prompt.clone(),
            phase: SchedulePhase::OffDuty,
            end_time: next_end, // not meaningful in OffDuty
            next_start,
        })
    }
}

/// The pre-warn threshold for the current window.
fn pre_warn_time(cs: &CycleState) -> OffsetDateTime {
    cs.end_time - time::Duration::minutes(cs.pre_warn_minutes)
}

/// The final-warn threshold for the current window.
fn final_warn_time(cs: &CycleState) -> OffsetDateTime {
    cs.end_time - time::Duration::minutes(cs.final_warn_minutes)
}

/// Given the current cycle state and wall-clock `now`, determine the next
/// transition that should fire (if any).
fn compute_transition(cs: &CycleState, now: OffsetDateTime) -> Option<Transition> {
    match cs.phase {
        SchedulePhase::OffDuty if now >= cs.next_start => Some(Transition::Start),
        SchedulePhase::Working if now >= pre_warn_time(cs) && !matches!(cs.spec, Spec::Always) => {
            Some(Transition::PreWarn)
        }
        SchedulePhase::PreWarned if now >= final_warn_time(cs) => Some(Transition::FinalWarn),
        SchedulePhase::FinalWarned if now >= cs.end_time => Some(Transition::End),
        _ => None,
    }
}

/// The End-boundary decision: the evaluator is authoritative there.
/// Merged or overlapping windows (adjacent daily windows, interval length
/// >= period) still cover `now`, so the nominal end of one window is not a
/// real end — returns the covering window's end so the shift continues
/// instead of flipping the agent off and back on.
fn end_covered_by_next(
    spec: &crate::work_schedule::spec::Spec,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    crate::work_schedule::eval::window_status(spec, now)
        .filter(|st| st.inside)
        .map(|st| st.next_end)
}

/// Recompute `next_start` after an End transition.
fn recompute_start_after_end(cs: &mut CycleState, now: OffsetDateTime) {
    if let Some(st) = crate::work_schedule::eval::window_status(&cs.spec, now) {
        cs.next_start = st.next_start;
    } else {
        // Unreachable for a validated spec; keep the tick loop safe.
        warn!(agent = %cs.agent_id, "recompute_start_after_end: eval failed, deferring 1h");
        cs.next_start = now + time::Duration::hours(1);
    }
}

// ---------------------------------------------------------------------------
// Phase executor
// ---------------------------------------------------------------------------

/// Cancel the current round token for an agent (if any). Mirrors the logic
/// in `interrupt_agent` but without auth checks — the engine is internal.
async fn interrupt_round(state: &SharedState, id: &AgentId) {
    let round_cancel = {
        let registry = state.registry.read().await;
        registry
            .get(id)
            .and_then(|e| e.as_live().map(|l| l.agent.round_cancel.clone()))
    };
    if let Some(slot) = round_cancel {
        if let Ok(guard) = slot.lock() {
            if let Some(rc) = guard.clone() {
                rc.cancel();
            }
        }
    }
}

/// The wake prompt's built-in text, sent at every shift start; a
/// per-schedule custom (when set) is appended after it, never replaces it.
pub(crate) const DEFAULT_WAKE_PROMPT: &str = "You are on duty — check your inbox for messages received while you were off duty, and resume any work left unfinished at the end of your last shift.";

/// Execute the Start transition: set on-duty, push wake prompt to inbox,
/// and notify the agent. The agent pulls ALL undelivered direct messages
/// (buffered off-duty messages + the wake prompt) together on wake.
async fn execute_start(state: &SharedState, cs: &CycleState) {
    state.duty.set(cs.agent_id.clone(), DutyStatus::OnDuty);
    info!(agent = %cs.agent_id, "schedule: start of shift");

    // The sent body is the built-in default followed by the custom text;
    // an empty or whitespace custom means the default alone — the same
    // append-not-replace contract as the final-warn prompt.
    let custom = cs.wake_prompt.trim();
    let body = if custom.is_empty() {
        DEFAULT_WAKE_PROMPT.to_string()
    } else {
        format!("{DEFAULT_WAKE_PROMPT}\n{custom}")
    };
    // Push the wake prompt to the inbox so it is pulled alongside any
    // buffered off-duty messages in one atomic pull.
    if let Some(store) = state.inboxes.get() {
        store
            .push(
                cs.agent_id.clone(),
                crate::inbox::BufferedEvent {
                    timestamp: time::OffsetDateTime::now_utc(),
                    source: "system".to_string(),
                    body,
                },
            )
            .await;
    } else {
        warn!(agent = %cs.agent_id, "inbox store not installed; wake prompt not delivered");
        return;
    }

    // Notify the agent to wake and pull from inbox.
    let notify = {
        let registry = state.registry.read().await;
        registry
            .get(&cs.agent_id)
            .and_then(|e| e.as_live())
            .map(|l| l.agent.notify.clone())
    };
    match notify {
        Some(n) => n.notify_one(),
        None => {
            warn!(agent = %cs.agent_id, "agent not live for schedule start; message buffered to inbox")
        }
    }
}

/// Execute the PreWarn transition.
async fn execute_pre_warn(state: &SharedState, cs: &CycleState) {
    let msg = format!(
        "⏰ Your shift ends in {} minutes. Start wrapping up.",
        cs.pre_warn_minutes
    );
    info!(agent = %cs.agent_id, "schedule: pre-warn ({} min)", cs.pre_warn_minutes);
    if let Err(e) = crate::delivery::enqueue_prompt(state, &cs.agent_id, msg, "system").await {
        warn!(agent = %cs.agent_id, error = %e, "schedule: failed to enqueue pre-warn");
    }
}

/// Execute the FinalWarn transition.
async fn execute_final_warn(state: &SharedState, cs: &CycleState) {
    // The sent body is the built-in default followed by the custom prompt:
    // a custom is a template ({N} = minutes left), and no custom at all
    // — or a leftover empty string, the stored ''-means-default form
    // leaking through — means the default alone.
    let default = format!(
        "⏰ {} minutes until end of shift. Save your work now.",
        cs.final_warn_minutes
    );
    let msg = match cs.final_warn_prompt.as_deref().map(str::trim) {
        Some(custom) if !custom.is_empty() => format!(
            "{default}\n{}",
            custom.replace("{N}", &cs.final_warn_minutes.to_string())
        ),
        _ => default,
    };
    info!(agent = %cs.agent_id, "schedule: final-warn ({} min)", cs.final_warn_minutes);
    if let Err(e) = crate::delivery::enqueue_prompt(state, &cs.agent_id, msg, "system").await {
        warn!(agent = %cs.agent_id, error = %e, "schedule: failed to enqueue final-warn");
    }
}

/// Execute the End transition: set off-duty, interrupt round.
async fn execute_end(state: &SharedState, cs: &CycleState) {
    state.duty.set(cs.agent_id.clone(), DutyStatus::OffDuty);
    info!(agent = %cs.agent_id, "schedule: end of shift (off-duty)");
    interrupt_round(state, &cs.agent_id).await;
}

// ---------------------------------------------------------------------------
// Engine task
// ---------------------------------------------------------------------------

/// Spawn the engine as a background task. Returns immediately. No-ops (with a
/// warning) when the work-schedule store is not installed — there is nothing
/// to schedule and no notify handle to wake on.
pub fn spawn(state: SharedState) {
    let Some(store) = state.work_schedules.get() else {
        warn!("work schedule engine not started: no store installed");
        return;
    };
    let notify = store.engine_notify().clone();
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        run(state, notify, shutdown).await;
    });
}

/// The engine loop: event-driven, with a coarse wall-clock tick as the
/// timer half.
///
/// - **Cold start**: the initial recompute before the first `select!`
///   loads the DB and runs the restart recovery (`init_cycle` sets duty
///   from the current wall clock) without waiting for any external signal.
/// - **Store mutations** wake the engine via `notify`; the recompute that
///   follows re-reads the DB, so schedule edits take effect immediately.
/// - **The tick arm** only compares the wall clock against the cached
///   `next_deadline` — a pure in-memory check. A recompute (the only DB
///   read) runs when the deadline is due, so the steady state is zero
///   queries; with no deadline (`None`) the comparison is never true and
///   the engine parks on notify/shutdown alone. See [`TICK_INTERVAL`] for
///   the precision and single-writer caveats this rests on.
/// - `notified()` is created fresh inside `select!` each iteration; tokio
///   stores a permit when no waiter is registered, so a mutation landing
///   mid-recompute wakes the *next* iteration. Worst case is a redundant
///   wake, never lost work — `recompute` is drain-all (same philosophy as
///   the agent-task notify arm).
async fn run(state: SharedState, notify: std::sync::Arc<Notify>, shutdown: CancellationToken) {
    info!("work schedule engine started");
    // First tick delayed by a full interval: the cold-start recompute just
    // ran, so an immediate tick would only duplicate its comparison.
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + TICK_INTERVAL, TICK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Map of schedule_id -> cycle state. Lives only in this task.
    let mut cycles: HashMap<String, CycleState> = HashMap::new();

    // Cold start: load schedules + run recovery before parking.
    let mut next_deadline = run_recompute(&state, &mut cycles).await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("work schedule engine shutting down");
                return;
            }
            _ = notify.notified() => {
                next_deadline = run_recompute(&state, &mut cycles).await;
            }
            _ = tick.tick() => {
                if next_deadline.is_some_and(|d| deadline_due(d, OffsetDateTime::now_utc())) {
                    next_deadline = run_recompute(&state, &mut cycles).await;
                }
            }
        }
    }
}

/// Run one recompute pass, logging errors, and return the next deadline.
/// A failed pass keeps the engine live: it returns a short retry anchor
/// (see [`RETRY_BACKOFF_SECS`]) instead of `None`, so the next tick
/// re-runs the pass rather than parking until a store mutation.
async fn run_recompute(
    state: &SharedState,
    cycles: &mut HashMap<String, CycleState>,
) -> Option<OffsetDateTime> {
    match recompute(state, cycles).await {
        Ok(deadline) => deadline,
        Err(e) => {
            error!(error = %e, "work schedule engine recompute failed");
            Some(retry_anchor(OffsetDateTime::now_utc()))
        }
    }
}

/// The earliest next transition timestamp across all tracked cycles, or
/// `None` when no cycles are tracked.
///
/// Parallel to [`compute_transition`] (which asks "is a threshold <= now?";
/// this asks "what IS the threshold"). Called at the END of `recompute`,
/// after the while-let drain — since `compute_transition` returned `None`,
/// every phase threshold is strictly in the future, so the returned deadline
/// needs no defensive clamping.
fn compute_next_deadline(cycles: &HashMap<String, CycleState>) -> Option<OffsetDateTime> {
    cycles
        .values()
        .map(|cs| match cs.phase {
            SchedulePhase::OffDuty => cs.next_start,
            SchedulePhase::Working => pre_warn_time(cs),
            SchedulePhase::PreWarned => final_warn_time(cs),
            SchedulePhase::FinalWarned => cs.end_time,
        })
        .min()
}

/// One recompute pass: sync schedules, fire due transitions, and return the
/// next deadline. The body (reload → sync → while-let multi-transition drain)
/// is unchanged from the polling era; only the driver around it changed.
async fn recompute(
    state: &SharedState,
    cycles: &mut HashMap<String, CycleState>,
) -> anyhow::Result<Option<OffsetDateTime>> {
    let Some(store) = state.work_schedules.get() else {
        return Ok(None); // work schedules not configured
    };

    let now = OffsetDateTime::now_utc();
    let root_id = {
        let registry = state.registry.read().await;
        registry
            .root_agent()
            .map(|(id, _)| id.clone())
    };
    let schedule = match store.get_singleton().await? {
        Some(s) if s.status == WorkScheduleStatus::Active => vec![s],
        _ => vec![],
    };
    let Some(root_id) = root_id else {
        // No root agent to carry duty transitions.
        cycles.clear();
        return Ok(None);
    };
    let active = schedule;

    // --- sync: remove deleted/paused schedules ---
    let active_ids: HashSet<&str> = active.iter().map(|s| s.id.as_str()).collect();
    cycles.retain(|id, _| active_ids.contains(id.as_str()));

    // --- process each active schedule ---
    for schedule in &active {
        let id = &schedule.id;

        // Initialize cycle state for new schedules (recovery: set duty).
        if !cycles.contains_key(id) {
            match init_cycle(schedule, &root_id, now) {
                Some(c) => {
                    match c.phase {
                        SchedulePhase::OffDuty => {
                            state.duty.set(c.agent_id.clone(), DutyStatus::OffDuty);
                        }
                        // Inside a work window on restart: flush the inbox
                        // and send a wake prompt (same as a normal Start),
                        // so buffered messages from the prior off-duty
                        // period are not stranded until the next cycle.
                        _ => {
                            execute_start(state, &c).await;
                        }
                    }
                    info!(
                        agent = %c.agent_id,
                        phase = ?c.phase,
                        "schedule: initialized (recovery)"
                    );
                    cycles.insert(id.clone(), c);
                }
                None => {
                    warn!(schedule_id = %id, "schedule: failed to init (cron parse error)");
                    continue;
                }
            }
        }

        let cs = cycles
            .get_mut(id)
            .expect("cycle inserted or continued above");

        // Detect schedule edits: re-init whenever any field that shapes the
        // cycle (window crons, warn thresholds, wake prompt) no longer
        // matches the snapshot.
        let edited = cs.spec != schedule.spec
            || cs.pre_warn_minutes != schedule.pre_warn_minutes as i64
            || cs.final_warn_minutes != schedule.final_warn_minutes as i64
            || cs.final_warn_prompt != schedule.final_warn_prompt
            || cs.wake_prompt != schedule.wake_prompt;
        if edited {
            if let Some(new_cs) = init_cycle(schedule, &root_id, now) {
                let was_on_duty = !matches!(cs.phase, SchedulePhase::OffDuty);
                let now_on_duty = !matches!(new_cs.phase, SchedulePhase::OffDuty);
                if !was_on_duty && now_on_duty {
                    // An edit that moves the window over "now" gets the same
                    // treatment as a cold start inside a window: flush the inbox
                    // and send a wake prompt, so messages buffered while off-duty
                    // are not stranded. Re-entry while already on-duty skips this
                    // (the wake prompt already went out for this window).
                    execute_start(state, &new_cs).await;
                } else {
                    // Off-ward edit: flip duty only, deliberately NOT
                    // interrupting the in-flight round. Departure is
                    // graceful by policy — PreWarn/FinalWarn nudge the
                    // agent to break on its own, and interrupt backs up
                    // only the natural End — while the duty flip already
                    // stops new work, so the running round ends naturally
                    // and the agent parks itself.
                    let duty = match new_cs.phase {
                        SchedulePhase::OffDuty => DutyStatus::OffDuty,
                        _ => DutyStatus::OnDuty,
                    };
                    state.duty.set(new_cs.agent_id.clone(), duty);
                }
                info!(schedule_id = %id, "schedule: re-initialized (edit detected)");
                *cs = new_cs;
            } else {
                warn!(
                    schedule_id = %id,
                    "schedule: edit detected but re-init failed (cron parse error); keeping old snapshot"
                );
            }
        }

        // Fire due transitions (loop in case multiple are past due).
        while let Some(trans) = compute_transition(cs, now) {
            match trans {
                Transition::Start => {
                    // A stalled-past-end start (suspend, OOM, NTP jump) is
                    // naturally detected by evaluating the spec at the fired
                    // start moment: if that moment's window already ends by
                    // `now`, skip the Start side-effects.
                    if let Some(st) = crate::work_schedule::eval::window_status(
                        &cs.spec,
                        cs.next_start,
                    ) {
                        if now >= st.next_end {
                            info!(agent = %cs.agent_id, "schedule: start window already ended, skipping");
                            state.duty.set(cs.agent_id.clone(), DutyStatus::OffDuty);
                            cs.phase = SchedulePhase::OffDuty;
                            recompute_start_after_end(cs, now);
                            continue;
                        }
                        cs.end_time = st.next_end;
                    }
                    execute_start(state, cs).await;
                    cs.phase = SchedulePhase::Working;
                }
                Transition::PreWarn => {
                    // An engine that stalled past the window end (long
                    // suspend) must not replay a "shift ends soon" nudge on
                    // catch-up: skip the side-effect, advance the phase.
                    // End below still takes the agent off-duty.
                    if now < cs.end_time {
                        execute_pre_warn(state, cs).await;
                    }
                    cs.phase = SchedulePhase::PreWarned;
                }
                Transition::FinalWarn => {
                    if now < cs.end_time {
                        execute_final_warn(state, cs).await;
                    }
                    cs.phase = SchedulePhase::FinalWarned;
                }
                Transition::End => {
                    // The evaluator is authoritative at the boundary: merged or
                    // overlapping windows (adjacent daily windows, interval
                    // length >= period) still cover `now`, so the nominal
                    // end of one window is not a real end. Continue the
                    // shift under the covering window's end instead of
                    // flipping the agent off and back on.
                    if let Some(covering_end) = end_covered_by_next(&cs.spec, now) {
                        info!(agent = %cs.agent_id, "schedule: window end covered by the next; continuing");
                        cs.end_time = covering_end;
                        cs.phase = SchedulePhase::Working;
                    } else {
                        execute_end(state, cs).await;
                        cs.phase = SchedulePhase::OffDuty;
                        recompute_start_after_end(cs, now);
                    }
                }
            }
        }
    }

    Ok(compute_next_deadline(cycles))
}

#[cfg(test)]
mod tests;
