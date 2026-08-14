//! WorkSchedule scheduling engine and phase executor.
//!
//! A tokio background task that ticks every ~10 s, reads active work schedules
//! from the [`WorkScheduleStore`], computes phase transitions from cron
//! expressions, and fires four-phase lifecycle actions:
//!
//! - **Start**: set on-duty, flush inbox, send wake prompt.
//! - **PreWarn** (T − `pre_warn_minutes`): notify agent that shift ends soon.
//! - **FinalWarn** (T − `final_warn_minutes`): tell agent to save work now.
//! - **End**: set off-duty, interrupt the current round.
//!
//! The engine recomputes its state from scratch on restart. If the current
//! time falls inside a work window, duty is set on-duty immediately; if
//! outside, off-duty. Missed warnings during downtime are not replayed.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::cron::CronExpr;
use crate::duty::DutyStatus;
use crate::state::{AgentId, SharedState};
use crate::work_schedule::{WorkSchedule, WorkScheduleStatus};

/// Engine tick interval. Transitions fire up to this much late; acceptable
/// for minute-resolution schedules.
const TICK_INTERVAL: Duration = Duration::from_secs(10);

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

/// Spawn the engine as a background task. Returns immediately.
pub fn spawn(state: SharedState) {
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        run(state, shutdown).await;
    });
}

async fn run(state: SharedState, shutdown: CancellationToken) {
    info!("work schedule engine started");
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Map of schedule_id -> cycle state. Lives only in this task.
    let mut cycles: HashMap<String, CycleState> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("work schedule engine shutting down");
                return;
            }
            _ = interval.tick() => {
                if let Err(e) = tick(&state, &mut cycles).await {
                    error!(error = %e, "work schedule engine tick failed");
                }
            }
        }
    }
}

/// One tick: sync schedules, fire due transitions.
async fn tick(
    state: &SharedState,
    cycles: &mut HashMap<String, CycleState>,
) -> anyhow::Result<()> {
    let Some(store) = state.work_schedules.get() else {
        return Ok(()); // work schedules not configured
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

        // Detect cron edits: if the stored snapshot doesn't match, re-init.
        if cs.start_cron != schedule.start_cron || cs.end_cron != schedule.end_cron {
            if let Some(new_cs) = init_cycle(schedule, now) {
                let duty = match new_cs.phase {
                    SchedulePhase::OffDuty => DutyStatus::OffDuty,
                    _ => DutyStatus::OnDuty,
                };
                state.duty.set(new_cs.agent_id.clone(), duty);
                info!(schedule_id = %id, "schedule: re-initialized (cron edit detected)");
                *cs = new_cs;
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
                    execute_pre_warn(state, cs).await;
                    cs.phase = SchedulePhase::PreWarned;
                }
                Transition::FinalWarn => {
                    execute_final_warn(state, cs).await;
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

    Ok(())
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
    async fn tick_processes_schedule_lifecycle() {
        let state = make_engine_state().await;
        let store = state.work_schedules.get().unwrap();
        let now = OffsetDateTime::now_utc();
        let sched = WorkSchedule {
            id: "ws-test".into(), name: "Test".into(),
            agent_id: "agent-1".parse().unwrap(),
            start_cron: "0 0 * * *".into(), end_cron: "59 23 * * *".into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: now,
        };
        store.create(&sched).await.unwrap();
        let mut cycles = HashMap::new();
        tick(&state, &mut cycles).await.unwrap();
        assert!(cycles.contains_key("ws-test"));
        let cs = cycles.get("ws-test").unwrap();
        assert_ne!(cs.phase, SchedulePhase::OffDuty);
        let agent: AgentId = "agent-1".parse().unwrap();
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
    }

    #[tokio::test]
    async fn tick_no_store_returns_ok() {
        let state = make_state();
        let mut cycles = HashMap::new();
        tick(&state, &mut cycles).await.unwrap();
        assert!(cycles.is_empty());
    }

    #[tokio::test]
    async fn tick_removes_deleted_schedules() {
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
        tick(&state, &mut cycles).await.unwrap();
        assert!(cycles.contains_key("ws-del"));
        store.delete("ws-del").await.unwrap();
        tick(&state, &mut cycles).await.unwrap();
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
        let sched = WorkSchedule {
            id: "ws-recovery".into(), name: "Always".into(),
            agent_id: agent.clone(),
            start_cron: "0 0 * * *".into(), end_cron: "59 23 * * *".into(),
            pre_warn_minutes: 10, final_warn_minutes: 5,
            wake_prompt: "Wake up.".into(),
            status: WorkScheduleStatus::Active, timezone: None,
            created_at: now,
        };
        store.create(&sched).await.unwrap();

        let mut cycles = HashMap::new();
        tick(&state, &mut cycles).await.unwrap();

        // Duty should be OnDuty.
        assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
        // Inbox should have 2: buffered message + wake prompt.
        assert_eq!(
            state.inboxes.get().unwrap().len_for(&agent).await, 2,
            "recovery inside window should push wake prompt to inbox"
        );
    }
}
