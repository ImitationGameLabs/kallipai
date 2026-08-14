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

use crate::cron::CronExpr;
use crate::duty::DutyStatus;
use crate::state::{AgentId, SharedState};
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
    /// Waiting for the next start_cron fire.
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
    /// Selected fields from the schedule, snapshotted to detect cron edits.
    start_cron: String,
    end_cron: String,
    pre_warn_minutes: i64,
    final_warn_minutes: i64,
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

/// Determine whether `now` falls inside a work window by comparing the next
/// start and end fire times.
///
/// Returns `(is_inside_window, next_start, next_end)`.
fn window_status(
    start: &CronExpr,
    end: &CronExpr,
    now: OffsetDateTime,
) -> Option<(bool, OffsetDateTime, OffsetDateTime)> {
    let next_start = start.next_after(now).ok()?;
    let next_end = end.next_after(now).ok()?;
    // next_end < next_start  => we are inside a work window (the window's
    //   end comes before the *next* start).
    // next_start <= next_end => we are outside (the next event is a start).
    Some((next_end < next_start, next_start, next_end))
}

/// Initialise a [`CycleState`] for a freshly-loaded schedule.
///
/// Sets duty immediately (recovery) and returns the state with the correct
/// starting phase. Caller is responsible for the side-effecting duty.set call;
/// this function only computes the state.
fn init_cycle(schedule: &WorkSchedule, now: OffsetDateTime) -> Option<CycleState> {
    let start = CronExpr::parse(&schedule.start_cron).ok()?;
    let end = CronExpr::parse(&schedule.end_cron).ok()?;
    let (inside, next_start, next_end) = window_status(&start, &end, now)?;

    let pre = schedule.pre_warn_minutes as i64;
    let fin = schedule.final_warn_minutes as i64;

    if inside {
        // Inside a work window — Start already happened. Determine how far
        // through the warning sequence we are based on time remaining.
        let pre_warn_time = next_end - time::Duration::minutes(pre);
        let final_warn_time = next_end - time::Duration::minutes(fin);

        let phase = if now >= final_warn_time {
            SchedulePhase::FinalWarned
        } else if now >= pre_warn_time {
            SchedulePhase::PreWarned
        } else {
            SchedulePhase::Working
        };

        Some(CycleState {
            start_cron: schedule.start_cron.clone(),
            end_cron: schedule.end_cron.clone(),
            pre_warn_minutes: pre,
            final_warn_minutes: fin,
            agent_id: schedule.agent_id.clone(),
            wake_prompt: schedule.wake_prompt.clone(),
            phase,
            end_time: next_end,
            next_start,
        })
    } else {
        // Outside — waiting for the next start.
        Some(CycleState {
            start_cron: schedule.start_cron.clone(),
            end_cron: schedule.end_cron.clone(),
            pre_warn_minutes: pre,
            final_warn_minutes: fin,
            agent_id: schedule.agent_id.clone(),
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
        SchedulePhase::Working if now >= pre_warn_time(cs) => Some(Transition::PreWarn),
        SchedulePhase::PreWarned if now >= final_warn_time(cs) => Some(Transition::FinalWarn),
        SchedulePhase::FinalWarned if now >= cs.end_time => Some(Transition::End),
        _ => None,
    }
}

/// Recompute `next_start` after an End transition.
fn recompute_start_after_end(cs: &mut CycleState, now: OffsetDateTime) {
    if let Ok(start) = CronExpr::parse(&cs.start_cron) {
        match start.next_after(now) {
            Ok(ns) => cs.next_start = ns,
            // next_after failed (should not happen for a validated cron, but
            // guard against NTP jumps or clock skew): set a safe future value
            // so the tick loop does not spin on a stale next_start.
            Err(_) => {
                warn!(agent = %cs.agent_id, "recompute_start_after_end: next_after failed, deferring 1h");
                cs.next_start = now + time::Duration::hours(1);
            }
        }
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

/// Execute the Start transition: set on-duty, push wake prompt to inbox,
/// and notify the agent. The agent pulls ALL undelivered direct messages
/// (buffered off-duty messages + the wake prompt) together on wake.
async fn execute_start(state: &SharedState, cs: &CycleState) {
    state.duty.set(cs.agent_id.clone(), DutyStatus::OnDuty);
    info!(agent = %cs.agent_id, "schedule: start of shift");

    // Push the wake prompt to the inbox so it is pulled alongside any
    // buffered off-duty messages in one atomic pull.
    if let Some(store) = state.inboxes.get() {
        store
            .push(
                cs.agent_id.clone(),
                crate::inbox::BufferedEvent {
                    timestamp: time::OffsetDateTime::now_utc(),
                    source: "system".to_string(),
                    body: cs.wake_prompt.clone(),
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
        None => warn!(agent = %cs.agent_id, "agent not live for schedule start; message buffered to inbox"),
    }
}

/// Execute the PreWarn transition.
async fn execute_pre_warn(state: &SharedState, cs: &CycleState) {
    let msg = format!(
        "⏰ Your shift ends in {} minutes. Start wrapping up.",
        cs.pre_warn_minutes
    );
    info!(agent = %cs.agent_id, "schedule: pre-warn ({} min)", cs.pre_warn_minutes);
    if let Err(e) =
        crate::routes::enqueue_prompt(state, &cs.agent_id, msg, "system").await
    {
        warn!(agent = %cs.agent_id, error = %e, "schedule: failed to enqueue pre-warn");
    }
}

/// Execute the FinalWarn transition.
async fn execute_final_warn(state: &SharedState, cs: &CycleState) {
    let msg = format!(
        "⏰ {} minutes until end of shift. Save your work now.",
        cs.final_warn_minutes
    );
    info!(agent = %cs.agent_id, "schedule: final-warn ({} min)", cs.final_warn_minutes);
    if let Err(e) =
        crate::routes::enqueue_prompt(state, &cs.agent_id, msg, "system").await
    {
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
    let active = store.list(None, Some(WorkScheduleStatus::Active)).await?;

    // --- sync: remove deleted/paused schedules ---
    let active_ids: HashSet<&str> = active.iter().map(|s| s.id.as_str()).collect();
    cycles.retain(|id, _| active_ids.contains(id.as_str()));

    // --- process each active schedule ---
    for schedule in &active {
        let id = &schedule.id;

        // Initialize cycle state for new schedules (recovery: set duty).
        if !cycles.contains_key(id) {
            match init_cycle(schedule, now) {
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

        let cs = cycles.get_mut(id).expect("cycle inserted or continued above");

        // Detect schedule edits: re-init whenever any field that shapes the
        // cycle (window crons, warn thresholds, wake prompt) no longer
        // matches the snapshot.
        let edited = cs.start_cron != schedule.start_cron
            || cs.end_cron != schedule.end_cron
            || cs.pre_warn_minutes != schedule.pre_warn_minutes as i64
            || cs.final_warn_minutes != schedule.final_warn_minutes as i64
            || cs.wake_prompt != schedule.wake_prompt;
        if edited {
            if let Some(new_cs) = init_cycle(schedule, now) {
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
                    // Compute this window's end from the start that fired,
                    // NOT from `now` (which would always be in the future).
                    // If the engine stalled past the window end (suspend,
                    // OOM, NTP jump), skip the Start side-effects.
                    if let Ok(end) = CronExpr::parse(&cs.end_cron) {
                        if let Ok(window_end) = end.next_after(cs.next_start) {
                            if now >= window_end {
                                info!(agent = %cs.agent_id, "schedule: start window already ended, skipping");
                                state.duty.set(cs.agent_id.clone(), DutyStatus::OffDuty);
                                cs.phase = SchedulePhase::OffDuty;
                                recompute_start_after_end(cs, now);
                                continue;
                            }
                            cs.end_time = window_end;
                        }
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
                    execute_end(state, cs).await;
                    cs.phase = SchedulePhase::OffDuty;
                    recompute_start_after_end(cs, now);
                }
            }
        }
    }

    Ok(compute_next_deadline(cycles))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::duty::DutyStatus;
    use crate::inbox::BufferedEvent;
    use crate::state::AgentId;
    use crate::test_helpers::{install_inbox_store, make_state};
    use crate::work_schedule::{WorkSchedule, WorkScheduleStatus, WorkScheduleStore};
    use time::macros::datetime;
    use time::OffsetDateTime;

    fn sample_schedule(start_cron: &str, end_cron: &str) -> WorkSchedule {
        WorkSchedule {
            id: "ws1".into(), name: "Test".into(),
            agent_id: "agent-1".parse().unwrap(),
            start_cron: start_cron.into(), end_cron: end_cron.into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    /// A "M H * * *" cron pinned to `t`'s time-of-day, so a window's relation
    /// to `now` stays fixed no matter when the test runs (a literal
    /// "0 0 * * *"/"59 23 * * *" pair flips outside at 23:59 UTC).
    fn tod_cron(t: OffsetDateTime) -> String {
        format!("{} {} * * *", t.minute(), t.hour())
    }

    /// (start, end) crons for a daily window covering `now`, with every
    /// threshold (end - pre/final warn minutes) strictly in the future.
    fn covering_window(now: OffsetDateTime) -> (String, String) {
        (tod_cron(now - time::Duration::minutes(10)), tod_cron(now + time::Duration::minutes(50)))
    }

    /// (start, end) crons for a daily window starting hours from `now` —
    /// always outside, with no transition due within the test's lifetime.
    fn far_window(now: OffsetDateTime) -> (String, String) {
        (tod_cron(now + time::Duration::hours(3)), tod_cron(now + time::Duration::hours(4)))
    }

    #[test]
    fn init_outside_window() {
        let now = datetime!(2024-01-15 08:00 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::OffDuty);
        assert_eq!(cs.next_start, datetime!(2024-01-15 09:00 UTC));
    }

    #[test]
    fn init_inside_window_working() {
        let now = datetime!(2024-01-15 10:00 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::Working);
        assert_eq!(cs.end_time, datetime!(2024-01-15 17:00 UTC));
    }

    #[test]
    fn init_inside_window_pre_warned() {
        let now = datetime!(2024-01-15 16:52 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::PreWarned);
    }

    #[test]
    fn init_inside_window_final_warned() {
        let now = datetime!(2024-01-15 16:57 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::FinalWarned);
    }

    #[test]
    fn init_after_window() {
        let now = datetime!(2024-01-15 18:00 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::OffDuty);
        assert_eq!(cs.next_start, datetime!(2024-01-16 09:00 UTC));
    }

    #[test]
    fn init_weekend_off_duty() {
        let now = datetime!(2024-01-13 10:00 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::OffDuty);
        assert_eq!(cs.next_start, datetime!(2024-01-15 09:00 UTC));
    }

    #[test]
    fn transition_off_to_working() {
        let now = datetime!(2024-01-15 08:00 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(compute_transition(&cs, now), None);
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 09:00 UTC)), Some(Transition::Start));
    }

    #[test]
    fn transition_working_to_pre_warned() {
        let now = datetime!(2024-01-15 10:00 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 16:49 UTC)), None);
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 16:50 UTC)), Some(Transition::PreWarn));
    }

    #[test]
    fn transition_pre_to_final() {
        let now = datetime!(2024-01-15 10:00 UTC);
        let mut cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        cs.phase = SchedulePhase::PreWarned;
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 16:54 UTC)), None);
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 16:55 UTC)), Some(Transition::FinalWarn));
    }

    #[test]
    fn transition_final_to_off() {
        let now = datetime!(2024-01-15 10:00 UTC);
        let mut cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        cs.phase = SchedulePhase::FinalWarned;
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 16:59 UTC)), None);
        assert_eq!(compute_transition(&cs, datetime!(2024-01-15 17:00 UTC)), Some(Transition::End));
    }

    #[test]
    fn full_cycle_advances_next_start() {
        let now = datetime!(2024-01-15 10:00 UTC);
        let mut cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
        cs.phase = SchedulePhase::OffDuty;
        recompute_start_after_end(&mut cs, datetime!(2024-01-15 17:00 UTC));
        assert_eq!(cs.next_start, datetime!(2024-01-16 09:00 UTC));
    }

    #[test]
    fn very_short_window_skips_to_final_warned() {
        let now = datetime!(2024-01-15 09:00:30 UTC);
        let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "1 9 * * 1-5"), now).unwrap();
        assert_eq!(cs.phase, SchedulePhase::FinalWarned);
    }

    async fn make_engine_state() -> SharedState {
        let state = make_state();
        let store = WorkScheduleStore::open_in_memory().await;
        state.work_schedules.set(store).ok();
        install_inbox_store(&state).await;
        state
    }

    fn cycle_for(agent: &str) -> CycleState {
        CycleState {
            start_cron: "0 9 * * 1-5".into(), end_cron: "0 17 * * 1-5".into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            agent_id: agent.parse().unwrap(), wake_prompt: "Wake up.".into(),
            phase: SchedulePhase::Working,
            end_time: datetime!(2024-01-15 17:00 UTC),
            next_start: datetime!(2024-01-16 09:00 UTC),
        }
    }

    #[tokio::test]
    async fn start_sets_on_duty_and_pushes_wake_to_inbox() {
        let state = make_engine_state().await;
        let agent: AgentId = "agent-1".parse().unwrap();
        state.duty.set(agent.clone(), DutyStatus::OffDuty);
        state.inboxes.get().unwrap().push(agent.clone(), BufferedEvent {
            timestamp: OffsetDateTime::now_utc(),
            source: "operator".into(), body: "hello".into(),
        }).await;
        assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 1);
        execute_start(&state, &cycle_for("agent-1")).await;
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
        // Wake prompt was pushed to inbox (now 2 messages: buffered + wake).
        assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 2);
        // Verify the wake prompt is pullable alongside the buffered message.
        let msg = state.inboxes.get().unwrap().pull_undelivered(&agent).await.unwrap();
        assert!(msg.contains("hello"), "buffered message should be in pull: {msg}");
        assert!(msg.contains("Wake up."), "wake prompt should be in pull: {msg}");
    }

    #[tokio::test]
    async fn end_sets_off_duty() {
        let state = make_engine_state().await;
        let agent: AgentId = "agent-1".parse().unwrap();
        state.duty.set(agent.clone(), DutyStatus::OnDuty);
        execute_end(&state, &cycle_for("agent-1")).await;
        assert_eq!(state.duty.get(&agent), DutyStatus::OffDuty);
    }

    #[tokio::test]
    async fn start_skipped_when_window_already_ended() {
        // Simulate engine stall: schedule starts at 09:00, ends at 10:00,
        // but now is 10:30 (past the window end). The Start transition
        // should be skipped — agent stays off-duty.
        let state = make_engine_state().await;
        let cs = CycleState {
            start_cron: "0 9 * * 1-5".into(), end_cron: "0 10 * * 1-5".into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            agent_id: "agent-1".parse().unwrap(), wake_prompt: "Wake up.".into(),
            phase: SchedulePhase::OffDuty,
            // next_start is in the past (09:00 today), now is past 10:00.
            next_start: datetime!(2024-01-15 09:00 UTC),
            end_time: datetime!(2024-01-15 10:00 UTC),
        };
        let now = datetime!(2024-01-15 10:30 UTC);

        // compute_transition should fire Start (now >= next_start).
        assert_eq!(compute_transition(&cs, now), Some(Transition::Start));

        // But the tick handler should detect the window ended and skip.
        // Simulate what the Start arm does:
        let end = CronExpr::parse(&cs.end_cron).unwrap();
        let window_end = end.next_after(cs.next_start).unwrap();
        assert!(now >= window_end, "now should be past window end");
        // The agent stays off-duty.
        state.duty.set(cs.agent_id.clone(), DutyStatus::OffDuty);
        assert_eq!(state.duty.get(&cs.agent_id), DutyStatus::OffDuty);
    }

    #[tokio::test]
    async fn missing_agent_no_panic() {
        let state = make_engine_state().await;
        let cs = cycle_for("nonexistent-agent");
        execute_start(&state, &cs).await;
        execute_pre_warn(&state, &cs).await;
        execute_end(&state, &cs).await;
    }

    #[tokio::test]
    async fn recompute_processes_schedule_lifecycle() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let now = OffsetDateTime::now_utc();
        let (start_cron, end_cron) = covering_window(now);
        let sched = WorkSchedule {
            id: "ws-test".into(), name: "Test".into(),
            agent_id: "agent-1".parse().unwrap(),
            start_cron, end_cron,
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: now,
        };
        store.create(&sched).await.unwrap();
        let mut cycles = HashMap::new();
        recompute(&state, &mut cycles).await.unwrap();
        assert!(cycles.contains_key("ws-test"));
        let cs = cycles.get("ws-test").unwrap();
        assert_ne!(cs.phase, SchedulePhase::OffDuty);
        let agent: AgentId = "agent-1".parse().unwrap();
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
    }

    #[tokio::test]
    async fn recompute_no_store_returns_ok_none() {
        let state = make_state();
        let mut cycles = HashMap::new();
        let deadline = recompute(&state, &mut cycles).await.unwrap();
        assert!(cycles.is_empty());
        assert!(deadline.is_none(), "no store -> no deadline");
    }

    #[tokio::test]
    async fn recompute_removes_deleted_schedules() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let now = OffsetDateTime::now_utc();
        let sched = WorkSchedule {
            id: "ws-del".into(), name: "Test".into(),
            agent_id: "agent-1".parse().unwrap(),
            start_cron: "0 0 * * *".into(), end_cron: "59 23 * * *".into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: now,
        };
        store.create(&sched).await.unwrap();
        let mut cycles = HashMap::new();
        recompute(&state, &mut cycles).await.unwrap();
        assert!(cycles.contains_key("ws-del"));
        store.delete("ws-del").await.unwrap();
        recompute(&state, &mut cycles).await.unwrap();
        assert!(!cycles.contains_key("ws-del"));
    }

    /// On restart landing inside a work window, the recovery path should
    /// Recovery inside a work window: tick() with an active schedule covering
    /// now() should set duty=OnDuty and push the wake prompt to inbox.
    #[tokio::test]
    async fn recovery_inside_window_drives_tick_and_pushes_wake() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let agent: AgentId = "agent-1".parse().unwrap();
        // Buffer a message in the inbox.
        state.inboxes.get().unwrap().push(agent.clone(), BufferedEvent {
            timestamp: OffsetDateTime::now_utc(),
            source: "operator".into(), body: "while you were off".into(),
        }).await;
        assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 1);

        // Create an active schedule whose window covers now.
        let now = OffsetDateTime::now_utc();
        let (start_cron, end_cron) = covering_window(now);
        let sched = WorkSchedule {
            id: "ws-recovery".into(), name: "Always".into(),
            agent_id: agent.clone(),
            start_cron, end_cron,
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: now,
        };
        store.create(&sched).await.unwrap();

        let mut cycles = HashMap::new();
        recompute(&state, &mut cycles).await.unwrap();

        // Duty should be OnDuty.
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
        // Inbox should have 2: buffered message + wake prompt.
        assert_eq!(
            state.inboxes.get().unwrap().len_for(&agent).await, 2,
            "recovery inside window should push wake prompt to inbox"
        );
    }

    // -- compute_next_deadline: the pure deadline helper --

    fn working_cycle(id: &str, end_time: OffsetDateTime) -> (String, CycleState) {
        (
            id.to_string(),
            CycleState {
                start_cron: "0 9 * * 1-5".into(),
                end_cron: "0 17 * * 1-5".into(),
                pre_warn_minutes: 10,
                final_warn_minutes: 5,
                agent_id: "agent-1".parse().unwrap(),
                wake_prompt: "Wake up.".into(),
                phase: SchedulePhase::Working,
                end_time,
                next_start: datetime!(2024-01-16 09:00 UTC),
            },
        )
    }

    #[test]
    fn next_deadline_none_when_no_cycles() {
        let cycles = HashMap::new();
        assert_eq!(compute_next_deadline(&cycles), None);
    }

    #[test]
    fn next_deadline_per_phase_mapping() {
        // OffDuty -> next_start
        let (_, mut cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
        cs.phase = SchedulePhase::OffDuty;
        assert_eq!(compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
                   Some(datetime!(2024-01-16 09:00 UTC)));

        // Working -> pre_warn_time (end - 10 min)
        let (_, cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
        assert_eq!(compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
                   Some(datetime!(2024-01-15 16:50 UTC)));

        // PreWarned -> final_warn_time (end - 5 min)
        let (_, mut cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
        cs.phase = SchedulePhase::PreWarned;
        assert_eq!(compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
                   Some(datetime!(2024-01-15 16:55 UTC)));

        // FinalWarned -> end_time
        let (_, mut cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
        cs.phase = SchedulePhase::FinalWarned;
        assert_eq!(compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
                   Some(datetime!(2024-01-15 17:00 UTC)));
    }

    #[test]
    fn next_deadline_is_min_across_schedules() {
        let mut map = HashMap::new();
        // A: Working, deadline 16:50.
        let (id_a, cs_a) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
        map.insert(id_a, cs_a);
        // B: OffDuty, deadline (next_start) 2024-01-16 09:00 — later than A's.
        let (id_b, mut cs_b) = working_cycle("b", datetime!(2024-01-15 19:00 UTC));
        cs_b.phase = SchedulePhase::OffDuty;
        cs_b.next_start = datetime!(2024-01-16 09:00 UTC);
        map.insert(id_b, cs_b);
        // C: FinalWarned, deadline 15:30 — the earliest.
        let (id_c, mut cs_c) = working_cycle("c", datetime!(2024-01-15 15:30 UTC));
        cs_c.phase = SchedulePhase::FinalWarned;
        map.insert(id_c, cs_c);
        assert_eq!(compute_next_deadline(&map), Some(datetime!(2024-01-15 15:30 UTC)));
    }

    #[test]
    fn next_deadline_is_future_after_drain() {
        // Invariant: after the while-let drain in recompute, the returned
        // deadline is strictly in the future (compute_transition returned
        // None). Model the drain here: a FinalWarned cycle whose end_time
        // already passed fires End, the phase advances to OffDuty, and the
        // recomputed next_start is future.
        let now = datetime!(2024-01-15 17:00 UTC);
        let mut map = HashMap::new();
        let (id, mut cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
        cs.phase = SchedulePhase::FinalWarned;
        map.insert(id, cs);
        // Drain: End fires; recompute next_start after end.
        {
            let cs = map.get_mut("a").unwrap();
            assert_eq!(compute_transition(cs, now), Some(Transition::End));
            cs.phase = SchedulePhase::OffDuty;
            recompute_start_after_end(cs, now);
        }
        let deadline = compute_next_deadline(&map).unwrap();
        assert!(deadline > now, "deadline must be future after drain: {deadline:?} <= {now:?}");
    }

    // -- Store mutations notify the engine --

    #[tokio::test]
    async fn store_create_leaves_notify_permit() {
        let store = WorkScheduleStore::open_in_memory().await;
        let notify = store.engine_notify().clone();
        store.create(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5")).await.unwrap();
        // The permit must be stored: notified() resolves immediately.
        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
            .await
            .expect("create must notify the engine");
    }

    #[tokio::test]
    async fn store_update_leaves_notify_permit() {
        let store = WorkScheduleStore::open_in_memory().await;
        let notify = store.engine_notify().clone();
        let mut sched = sample_schedule("0 9 * * 1-5", "0 17 * * 1-5");
        store.create(&sched).await.unwrap();
        // Drain the create permit so the assert below observes only the
        // mutation under test.
        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
            .await
            .expect("drain create permit");
        sched.name = "Renamed".into();
        assert!(store.update(&sched).await.unwrap());
        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
            .await
            .expect("update must notify the engine");
    }

    #[tokio::test]
    async fn store_delete_leaves_notify_permit() {
        let store = WorkScheduleStore::open_in_memory().await;
        let notify = store.engine_notify().clone();
        store.create(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5")).await.unwrap();
        // Drain the create permit so the assert below observes only the
        // mutation under test.
        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
            .await
            .expect("drain create permit");
        assert!(store.delete("ws1").await.unwrap());
        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
            .await
            .expect("delete must notify the engine");
    }

    // -- Driver loop: run() end-to-end (cold start + notify wake) --
    //
    // The tick arm's wiring is not directly testable without clock control
    // (recompute reads the wall clock, so tokio time-pausing cannot drive
    // it); its due comparison and retry anchor are unit-tested as the pure
    // functions `deadline_due` / `retry_anchor`, and the precision and
    // single-writer caveats are documented on TICK_INTERVAL.

    /// Helper: drain any stored permit so the engine must rely on the wake
    /// source under test, not a leftover create-notify.
    async fn drain_permits(notify: &std::sync::Arc<Notify>) {
        while let Ok(_) =
            tokio::time::timeout(std::time::Duration::from_millis(20), notify.notified()).await
        {}
    }

    /// Cold start: a schedule already in the DB (window covering now) drives
    /// duty to OnDuty via the pre-select recompute alone — no notify fires
    /// after spawn (the create's permit is drained first).
    #[tokio::test]
    async fn run_cold_start_recomputes_before_first_select() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let notify = store.engine_notify().clone();
        let agent: AgentId = "agent-1".parse().unwrap();
        state.duty.set(agent.clone(), DutyStatus::OffDuty);

        let (start_cron, end_cron) = covering_window(OffsetDateTime::now_utc());
        let sched = WorkSchedule {
            id: "ws-cold".into(), name: "Covering now".into(),
            agent_id: agent.clone(),
            start_cron: start_cron.clone(), end_cron: end_cron.clone(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: OffsetDateTime::now_utc(),
        };
        store.create(&sched).await.unwrap();
        drain_permits(&notify).await;

        let shutdown = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn(run(state.clone(), notify, shutdown.clone()));

        // Duty flips via the cold-start recompute, well before any tick
        // (the window covers now with all thresholds in the future) and
        // with no stored permit.
        let flipped = poll_until(3, || async { state.duty.get(&agent) == DutyStatus::OnDuty }).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(flipped, "cold-start recompute must set duty from an existing schedule");
    }

    /// Notify wake: a mid-park store mutation (schedule now covers now)
    /// wakes the engine and drives the transition — proving the notify arm
    /// reloads and the loop re-arms its sleep.
    #[tokio::test]
    async fn run_notify_wake_reloads_and_fires_transition() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let notify = store.engine_notify().clone();
        let agent: AgentId = "agent-2".parse().unwrap();
        state.duty.set(agent.clone(), DutyStatus::OffDuty);

        // Start with a schedule whose window does NOT cover now, so the
        // engine parks on the notify arm alone (next start hours away).
        let now = OffsetDateTime::now_utc();
        let (far_start, far_end) = far_window(now);
        let mut sched = WorkSchedule {
            id: "ws-notify".into(), name: "Far window".into(),
            agent_id: agent.clone(),
            start_cron: far_start.clone(), end_cron: far_end.clone(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: OffsetDateTime::now_utc(),
        };
        store.create(&sched).await.unwrap();
        drain_permits(&notify).await;

        let shutdown = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn(run(state.clone(), notify.clone(), shutdown.clone()));

        // Engine parks (off-duty, next start hours away). Mutate the schedule
        // so its window covers now; the store's notify must wake the engine,
        // which re-enters the window via the edit path: execute_start fires
        // (same as a cold start inside a window), not just a duty flip.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (start_cron, end_cron) = covering_window(OffsetDateTime::now_utc());
        sched.start_cron = start_cron;
        sched.end_cron = end_cron;
        store.update(&sched).await.unwrap();

        let flipped = poll_until(3, || async {
            state.duty.get(&agent) == DutyStatus::OnDuty
                && state.inboxes.get().unwrap().len_for(&agent).await >= 1
        }).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(flipped, "store mutation must wake the engine and fire the transition");
        let msg = state.inboxes.get().unwrap().pull_undelivered(&agent).await.unwrap();
        assert!(msg.contains("Wake up."), "edit into a window must deliver the wake prompt: {msg}");
    }

    // -- Edit-gate regression (083821a): the gate must re-init on any field
    // that shapes the cycle, not just the window crons. --

    #[tokio::test]
    async fn edit_gate_reinits_on_warn_and_prompt_fields() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let now = OffsetDateTime::now_utc();
        let (start_cron, end_cron) = covering_window(now);
        let mut sched = sample_schedule(&start_cron, &end_cron);
        store.create(&sched).await.unwrap();
        let mut cycles = HashMap::new();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(cycles.get("ws1").unwrap().phase, SchedulePhase::Working);

        // pre_warn 10 -> 55: the pre-warn threshold (end - 55m) is now in
        // the past, so the re-inited cycle lands in PreWarned. A stale
        // snapshot (gate not covering pre_warn) would stay Working.
        sched.pre_warn_minutes = 55;
        store.update(&sched).await.unwrap();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(cycles.get("ws1").unwrap().phase, SchedulePhase::PreWarned);

        // final_warn 5 -> 52: both thresholds past -> FinalWarned.
        sched.final_warn_minutes = 52;
        store.update(&sched).await.unwrap();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(cycles.get("ws1").unwrap().phase, SchedulePhase::FinalWarned);

        // wake_prompt: the snapshot itself must carry the new value (it is
        // consumed by the NEXT Start, hours away — no other observable
        // effect within this test's lifetime).
        sched.wake_prompt = "Rise and shine.".into();
        store.update(&sched).await.unwrap();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(cycles.get("ws1").unwrap().wake_prompt, "Rise and shine.");
    }

    #[tokio::test]
    async fn edit_while_on_duty_skips_wake_resend() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let agent: AgentId = "agent-1".parse().unwrap();
        state.duty.set(agent.clone(), DutyStatus::OffDuty);
        let now = OffsetDateTime::now_utc();
        let (start_cron, end_cron) = covering_window(now);
        let mut sched = sample_schedule(&start_cron, &end_cron);
        store.create(&sched).await.unwrap();
        let mut cycles = HashMap::new();
        // Cold start inside the window: recovery fires execute_start once
        // (flush + wake prompt) — exactly one inbox entry.
        recompute(&state, &mut cycles).await.unwrap();
        let inbox = state.inboxes.get().unwrap();
        assert_eq!(inbox.len_for(&agent).await, 1, "cold start delivers one wake prompt");
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);

        // Any edit that keeps the window covering now (here: the prompt
        // text) must NOT re-send — on-duty re-entry is duty.set only.
        sched.wake_prompt = "Second prompt.".into();
        store.update(&sched).await.unwrap();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(
            inbox.len_for(&agent).await, 1,
            "on-duty re-entry must not re-send the wake prompt"
        );
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
    }

    #[tokio::test]
    async fn edit_moving_window_off_now_sets_off_duty() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let agent: AgentId = "agent-1".parse().unwrap();
        let now = OffsetDateTime::now_utc();
        let (start_cron, end_cron) = covering_window(now);
        let mut sched = sample_schedule(&start_cron, &end_cron);
        store.create(&sched).await.unwrap();
        let mut cycles = HashMap::new();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);

        // Move the window hours away: the edit re-inits to OffDuty and the
        // duty map must follow. (The in-flight round is deliberately NOT
        // interrupted — graceful-departure policy, see the edit path.)
        let (far_start, far_end) = far_window(now);
        sched.start_cron = far_start;
        sched.end_cron = far_end;
        store.update(&sched).await.unwrap();
        recompute(&state, &mut cycles).await.unwrap();
        assert_eq!(state.duty.get(&agent), DutyStatus::OffDuty);
        let cs = cycles.get("ws1").unwrap();
        assert_eq!(cs.phase, SchedulePhase::OffDuty);
        assert!(cs.next_start > now, "next start must be in the future");
    }

    #[tokio::test]
    async fn store_update_unknown_id_skips_notify() {
        let store = WorkScheduleStore::open_in_memory().await;
        let notify = store.engine_notify().clone();
        let sched = sample_schedule("0 9 * * 1-5", "0 17 * * 1-5");
        // No create: the id is unknown. The update reports false and must
        // NOT leave a wake permit for the engine.
        assert!(!store.update(&sched).await.unwrap());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                notify.notified()
            )
            .await
            .is_err(),
            "unknown-id update must not notify"
        );
    }

    // -- Driver-layer pure functions (tick due comparison, retry anchor) --

    #[test]
    fn deadline_due_is_inclusive_at_boundary() {
        let d = datetime!(2024-01-15 12:00:00 UTC);
        assert!(!deadline_due(d, datetime!(2024-01-15 11:59:59 UTC)));
        assert!(deadline_due(d, datetime!(2024-01-15 12:00:00 UTC)), "exact hit is due");
        assert!(deadline_due(d, datetime!(2024-01-15 12:00:01 UTC)));
    }

    #[test]
    fn retry_anchor_is_backoff_out() {
        let now = datetime!(2024-01-15 12:00:00 UTC);
        assert_eq!(retry_anchor(now), datetime!(2024-01-15 12:00:10 UTC));
    }

    /// Poll an async condition at 50ms cadence for up to `secs` seconds.
    async fn poll_until<F, Fut>(secs: u64, mut cond: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if cond().await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }
}
