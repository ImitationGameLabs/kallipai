use super::*;
use crate::duty::DutyStatus;
use crate::inbox::BufferedEvent;
use crate::state::AgentId;
use crate::test_helpers::{install_inbox_store, make_state};
use crate::work_schedule::{WorkSchedule, WorkScheduleStatus, WorkScheduleStore};
use time::OffsetDateTime;
use time::macros::datetime;

fn sample_schedule(start_cron: &str, end_cron: &str) -> WorkSchedule {
    WorkSchedule {
        id: "ws1".into(),
        name: "Test".into(),
        agent_id: "agent-1".parse().unwrap(),
        start_cron: start_cron.into(),
        end_cron: end_cron.into(),
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "Wake up.".into(),
        status: WorkScheduleStatus::Active,
        timezone: None,
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
    (
        tod_cron(now - time::Duration::minutes(10)),
        tod_cron(now + time::Duration::minutes(50)),
    )
}

/// (start, end) crons for a daily window starting hours from `now` —
/// always outside, with no transition due within the test's lifetime.
fn far_window(now: OffsetDateTime) -> (String, String) {
    (
        tod_cron(now + time::Duration::hours(3)),
        tod_cron(now + time::Duration::hours(4)),
    )
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
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 09:00 UTC)),
        Some(Transition::Start)
    );
}

#[test]
fn transition_working_to_pre_warned() {
    let now = datetime!(2024-01-15 10:00 UTC);
    let cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 16:49 UTC)),
        None
    );
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 16:50 UTC)),
        Some(Transition::PreWarn)
    );
}

#[test]
fn transition_pre_to_final() {
    let now = datetime!(2024-01-15 10:00 UTC);
    let mut cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
    cs.phase = SchedulePhase::PreWarned;
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 16:54 UTC)),
        None
    );
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 16:55 UTC)),
        Some(Transition::FinalWarn)
    );
}

#[test]
fn transition_final_to_off() {
    let now = datetime!(2024-01-15 10:00 UTC);
    let mut cs = init_cycle(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"), now).unwrap();
    cs.phase = SchedulePhase::FinalWarned;
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 16:59 UTC)),
        None
    );
    assert_eq!(
        compute_transition(&cs, datetime!(2024-01-15 17:00 UTC)),
        Some(Transition::End)
    );
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
        start_cron: "0 9 * * 1-5".into(),
        end_cron: "0 17 * * 1-5".into(),
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        agent_id: agent.parse().unwrap(),
        wake_prompt: "Wake up.".into(),
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
    state
        .inboxes
        .get()
        .unwrap()
        .push(
            agent.clone(),
            BufferedEvent {
                timestamp: OffsetDateTime::now_utc(),
                source: "operator".into(),
                body: "hello".into(),
            },
        )
        .await;
    assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 1);
    execute_start(&state, &cycle_for("agent-1")).await;
    assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
    // Wake prompt was pushed to inbox (now 2 messages: buffered + wake).
    assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 2);
    // Verify the wake prompt is pullable alongside the buffered message.
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&agent)
        .await
        .unwrap();
    assert!(
        msg.contains("hello"),
        "buffered message should be in pull: {msg}"
    );
    assert!(
        msg.contains("Wake up."),
        "wake prompt should be in pull: {msg}"
    );
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
        start_cron: "0 9 * * 1-5".into(),
        end_cron: "0 10 * * 1-5".into(),
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        agent_id: "agent-1".parse().unwrap(),
        wake_prompt: "Wake up.".into(),
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
        id: "ws-test".into(),
        name: "Test".into(),
        agent_id: "agent-1".parse().unwrap(),
        start_cron,
        end_cron,
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "Wake up.".into(),
        status: WorkScheduleStatus::Active,
        timezone: None,
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
        id: "ws-del".into(),
        name: "Test".into(),
        agent_id: "agent-1".parse().unwrap(),
        start_cron: "0 0 * * *".into(),
        end_cron: "59 23 * * *".into(),
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "Wake up.".into(),
        status: WorkScheduleStatus::Active,
        timezone: None,
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
    state
        .inboxes
        .get()
        .unwrap()
        .push(
            agent.clone(),
            BufferedEvent {
                timestamp: OffsetDateTime::now_utc(),
                source: "operator".into(),
                body: "while you were off".into(),
            },
        )
        .await;
    assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 1);

    // Create an active schedule whose window covers now.
    let now = OffsetDateTime::now_utc();
    let (start_cron, end_cron) = covering_window(now);
    let sched = WorkSchedule {
        id: "ws-recovery".into(),
        name: "Always".into(),
        agent_id: agent.clone(),
        start_cron,
        end_cron,
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "Wake up.".into(),
        status: WorkScheduleStatus::Active,
        timezone: None,
        created_at: now,
    };
    store.create(&sched).await.unwrap();

    let mut cycles = HashMap::new();
    recompute(&state, &mut cycles).await.unwrap();

    // Duty should be OnDuty.
    assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);
    // Inbox should have 2: buffered message + wake prompt.
    assert_eq!(
        state.inboxes.get().unwrap().len_for(&agent).await,
        2,
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
    assert_eq!(
        compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
        Some(datetime!(2024-01-16 09:00 UTC))
    );

    // Working -> pre_warn_time (end - 10 min)
    let (_, cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
    assert_eq!(
        compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
        Some(datetime!(2024-01-15 16:50 UTC))
    );

    // PreWarned -> final_warn_time (end - 5 min)
    let (_, mut cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
    cs.phase = SchedulePhase::PreWarned;
    assert_eq!(
        compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
        Some(datetime!(2024-01-15 16:55 UTC))
    );

    // FinalWarned -> end_time
    let (_, mut cs) = working_cycle("a", datetime!(2024-01-15 17:00 UTC));
    cs.phase = SchedulePhase::FinalWarned;
    assert_eq!(
        compute_next_deadline(&HashMap::from([("a".into(), cs.clone())])),
        Some(datetime!(2024-01-15 17:00 UTC))
    );
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
    assert_eq!(
        compute_next_deadline(&map),
        Some(datetime!(2024-01-15 15:30 UTC))
    );
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
    assert!(
        deadline > now,
        "deadline must be future after drain: {deadline:?} <= {now:?}"
    );
}

// -- Store mutations notify the engine --

#[tokio::test]
async fn store_create_leaves_notify_permit() {
    let store = WorkScheduleStore::open_in_memory().await;
    let notify = store.engine_notify().clone();
    store
        .create(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"))
        .await
        .unwrap();
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
    store
        .create(&sample_schedule("0 9 * * 1-5", "0 17 * * 1-5"))
        .await
        .unwrap();
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
        id: "ws-cold".into(),
        name: "Covering now".into(),
        agent_id: agent.clone(),
        start_cron: start_cron.clone(),
        end_cron: end_cron.clone(),
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "Wake up.".into(),
        status: WorkScheduleStatus::Active,
        timezone: None,
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
    assert!(
        flipped,
        "cold-start recompute must set duty from an existing schedule"
    );
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
        id: "ws-notify".into(),
        name: "Far window".into(),
        agent_id: agent.clone(),
        start_cron: far_start.clone(),
        end_cron: far_end.clone(),
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "Wake up.".into(),
        status: WorkScheduleStatus::Active,
        timezone: None,
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
    })
    .await;
    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    assert!(
        flipped,
        "store mutation must wake the engine and fire the transition"
    );
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&agent)
        .await
        .unwrap();
    assert!(
        msg.contains("Wake up."),
        "edit into a window must deliver the wake prompt: {msg}"
    );
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
    assert_eq!(
        inbox.len_for(&agent).await,
        1,
        "cold start delivers one wake prompt"
    );
    assert_eq!(state.duty.get(&agent), DutyStatus::OnDuty);

    // Any edit that keeps the window covering now (here: the prompt
    // text) must NOT re-send — on-duty re-entry is duty.set only.
    sched.wake_prompt = "Second prompt.".into();
    store.update(&sched).await.unwrap();
    recompute(&state, &mut cycles).await.unwrap();
    assert_eq!(
        inbox.len_for(&agent).await,
        1,
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
        tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified())
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
    assert!(
        deadline_due(d, datetime!(2024-01-15 12:00:00 UTC)),
        "exact hit is due"
    );
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
