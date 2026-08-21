//! Engine tests, spec-driven. Interval specs give exact windows, so the
//! phase thresholds (pre-warn/final-warn) are computed from concrete
//! anchors. The cron-era suite was rewritten with the native model.

use super::*;
use crate::state::AgentId;
use crate::test_helpers::{add_root, install_inbox_store, make_state};
use crate::work_schedule::spec::Spec;
use crate::work_schedule::{WorkSchedule, WorkScheduleStatus, WorkScheduleStore};
use time::macros::datetime;
use time::OffsetDateTime;

/// Fixed reference: window = 12:00..12:30 (every 1h, 30min shifts).
/// pre_warn = 10min -> 12:20; final_warn = 5min -> 12:25.
const ANCHOR: OffsetDateTime = datetime!(2026-08-21 12:00 UTC);

fn root() -> AgentId {
    "agent-1".parse().unwrap()
}

fn spec(every_h: u16, len_min: u16, anchor: OffsetDateTime) -> Spec {
    Spec::Interval {
        every_hours: every_h,
        length_min: len_min,
        anchor,
    }
}

fn sample(s: Spec) -> WorkSchedule {
    WorkSchedule {
        id: "ws1".into(),
        spec: s,
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "wake up".into(),
        status: WorkScheduleStatus::Active,
        created_at: OffsetDateTime::now_utc(),
    }
}

fn base_spec() -> Spec {
    spec(1, 30, ANCHOR)
}

#[test]
fn init_outside_window_is_off_duty() {
    let cs = init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:45 UTC))
        .expect("init");
    assert_eq!(cs.phase, SchedulePhase::OffDuty);
    assert_eq!(cs.next_start, datetime!(2026-08-21 13:00 UTC));
}

#[test]
fn init_inside_working() {
    let cs = init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC))
        .expect("init");
    assert_eq!(cs.phase, SchedulePhase::Working);
    assert_eq!(cs.end_time, datetime!(2026-08-21 12:30 UTC));
}

#[test]
fn init_inside_pre_warned() {
    let cs = init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:22 UTC))
        .expect("init");
    assert_eq!(cs.phase, SchedulePhase::PreWarned);
}

#[test]
fn init_inside_final_warned() {
    let cs = init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:27 UTC))
        .expect("init");
    assert_eq!(cs.phase, SchedulePhase::FinalWarned);
}

#[test]
fn transition_off_to_working() {
    let mut cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:45 UTC)).unwrap();
    cs.next_start = datetime!(2026-08-21 12:46 UTC); // force due now
    let t = compute_transition(&cs, datetime!(2026-08-21 12:46 UTC)).unwrap();
    assert_eq!(t, Transition::Start);
}

#[test]
fn transition_working_to_pre_warned() {
    let mut cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC)).unwrap();
    cs.phase = SchedulePhase::Working;
    let t = compute_transition(&cs, datetime!(2026-08-21 12:21 UTC)).unwrap();
    assert_eq!(t, Transition::PreWarn);
}

#[test]
fn transition_pre_to_final() {
    let mut cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC)).unwrap();
    cs.phase = SchedulePhase::PreWarned;
    let t = compute_transition(&cs, datetime!(2026-08-21 12:26 UTC)).unwrap();
    assert_eq!(t, Transition::FinalWarn);
}

#[test]
fn transition_final_to_off() {
    let mut cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC)).unwrap();
    cs.phase = SchedulePhase::FinalWarned;
    let t = compute_transition(&cs, datetime!(2026-08-21 12:31 UTC)).unwrap();
    assert_eq!(t, Transition::End);
}

#[test]
fn full_cycle_advances_next_start() {
    let mut cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC)).unwrap();
    cs.phase = SchedulePhase::FinalWarned;
    let now = datetime!(2026-08-21 12:31 UTC);
    assert_eq!(compute_transition(&cs, now).unwrap(), Transition::End);
    recompute_start_after_end(&mut cs, now);
    assert_eq!(cs.next_start, datetime!(2026-08-21 13:00 UTC));
}

#[test]
fn end_boundary_continues_when_windows_merge() {
    // length == period: continuous duty. At the nominal end of one window
    // (12:00+2h) the evaluator still covers `now`; the shift continues
    // under the covering end (14:00+2h) instead of going off-duty.
    let s = spec(2, 120, ANCHOR);
    let covering = end_covered_by_next(&s, datetime!(2026-08-21 14:00 UTC));
    assert_eq!(covering, Some(datetime!(2026-08-21 16:00 UTC)));
}

#[test]
fn end_boundary_off_duty_when_gap_follows() {
    // base spec: 30-minute windows with a 30-minute gap. At the end of
    // the 12:00 window nothing covers `now` — a real end.
    let covering = end_covered_by_next(&base_spec(), datetime!(2026-08-21 12:30 UTC));
    assert_eq!(covering, None);
}

async fn make_engine_state() -> SharedState {
    let state = make_state();
    install_inbox_store(&state).await;
    let store = WorkScheduleStore::open_in_memory().await;
    state.work_schedules.set(store).ok();
    let mut reg = state.registry.write().await;
    add_root(&mut reg, &root());
    drop(reg);
    state
}

async fn store_active(state: &SharedState, s: &WorkSchedule) {
    state
        .work_schedules
        .get()
        .expect("store installed")
        .create(s)
        .await
        .expect("create");
}

#[tokio::test]
async fn start_sets_on_duty_and_pushes_wake_to_inbox() {
    let state = make_engine_state().await;
    let cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC)).unwrap();
    execute_start(&state, &cs).await;
    assert_eq!(state.duty.get(&root()), DutyStatus::OnDuty);
    let entries = state
        .inboxes
        .get()
        .expect("inbox installed in test state")
        .list(&root(), &crate::inbox::InboxFilter::default())
        .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "wake up");
}

#[tokio::test]
async fn end_sets_off_duty() {
    let state = make_engine_state().await;
    let cs =
        init_cycle(&sample(base_spec()), &root(), datetime!(2026-08-21 12:05 UTC)).unwrap();
    state.duty.set(root(), DutyStatus::OnDuty);
    execute_end(&state, &cs).await;
    assert_eq!(state.duty.get(&root()), DutyStatus::OffDuty);
}

#[tokio::test]
async fn start_skipped_when_window_already_ended() {
    let state = make_engine_state().await;
    // The 13:00 window ended at 13:30; recompute at 14:30 must skip the
    // stale Start (no duty set) instead of replaying it.
    store_active(&state, &sample(spec(1, 30, ANCHOR))).await;
    let mut cycles = std::collections::HashMap::new();
    let now = datetime!(2026-08-21 14:30 UTC);
    let _ = recompute(&state, &mut cycles).await;
    assert_eq!(state.duty.get(&root()), DutyStatus::OffDuty);
    let _ = now;
}

#[tokio::test]
async fn missing_agent_no_panic() {
    let state = make_state(); // no root registered
    let store = WorkScheduleStore::open_in_memory().await;
    state.work_schedules.set(store).ok();
    let mut cycles = std::collections::HashMap::new();
    let r = recompute(&state, &mut cycles).await;
    assert!(r.is_ok());
    assert!(r.unwrap().is_none());
}

#[tokio::test]
async fn recompute_processes_schedule_lifecycle() {
    let state = make_engine_state().await;
    // A window that covers "now" (anchored an hour ago, 2h period, 90min
    // length): recompute initializes the cycle and sets on-duty.
    let now = OffsetDateTime::now_utc();
    let covering = spec(2, 90, now - time::Duration::hours(1));
    store_active(&state, &sample(covering)).await;
    let mut cycles = std::collections::HashMap::new();
    let deadline = recompute(&state, &mut cycles).await.unwrap();
    assert!(deadline.is_some());
    assert_eq!(state.duty.get(&root()), DutyStatus::OnDuty);
    assert!(cycles.contains_key("ws1"));
}

#[tokio::test]
async fn recompute_no_store_returns_ok_none() {
    let state = make_state();
    let mut cycles = std::collections::HashMap::new();
    let d = recompute(&state, &mut cycles).await.unwrap();
    assert!(d.is_none());
}

#[tokio::test]
async fn recompute_drops_paused_schedule_cycle() {
    let state = make_engine_state().await;
    let now = OffsetDateTime::now_utc();
    let covering = spec(2, 90, now - time::Duration::hours(1));
    let mut s = sample(covering.clone());
    s.status = WorkScheduleStatus::Paused;
    store_active(&state, &s).await;
    let mut cycles = std::collections::HashMap::new();
    cycles.insert(
        "ws1".to_string(),
        CycleState {
            spec: covering,
            pre_warn_minutes: 10,
            final_warn_minutes: 5,
            agent_id: root(),
            wake_prompt: "wake up".into(),
            phase: SchedulePhase::Working,
            end_time: now + time::Duration::minutes(30),
            next_start: now + time::Duration::hours(2),
        },
    );
    let _ = recompute(&state, &mut cycles).await;
    assert!(cycles.is_empty(), "paused schedule's cycle must be dropped");
}

#[tokio::test]
async fn recovery_inside_window_drives_start() {
    let state = make_engine_state().await;
    let now = OffsetDateTime::now_utc();
    let covering = spec(2, 90, now - time::Duration::hours(1));
    store_active(&state, &sample(covering)).await;
    let mut cycles = std::collections::HashMap::new();
    let _ = recompute(&state, &mut cycles).await;
    // Cold start inside a window = full start treatment (duty + wake).
    assert_eq!(state.duty.get(&root()), DutyStatus::OnDuty);
    let entries = state
        .inboxes
        .get()
        .expect("inbox installed")
        .list(&root(), &crate::inbox::InboxFilter::default())
        .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "wake up");
}

fn working_cycle(id: &str, end_time: OffsetDateTime) -> (String, CycleState) {
    (
        id.into(),
        CycleState {
            spec: base_spec(),
            pre_warn_minutes: 10,
            final_warn_minutes: 5,
            agent_id: root(),
            wake_prompt: "wake up".into(),
            phase: SchedulePhase::Working,
            end_time,
            next_start: end_time + time::Duration::hours(1),
        },
    )
}

#[test]
fn next_deadline_none_when_no_cycles() {
    let cycles = std::collections::HashMap::new();
    assert!(compute_next_deadline(&cycles).is_none());
}

#[test]
fn next_deadline_per_phase_mapping() {
    let end = datetime!(2026-08-21 12:30 UTC);
    let mk = |phase, cycles: &mut std::collections::HashMap<String, CycleState>| {
        let (k, mut cs) = working_cycle("ws1", end);
        cs.phase = phase;
        cycles.insert(k, cs);
    };
    let mut cycles = std::collections::HashMap::new();
    mk(SchedulePhase::Working, &mut cycles);
    assert_eq!(
        compute_next_deadline(&cycles),
        Some(datetime!(2026-08-21 12:20 UTC))
    );
    let mut cycles = std::collections::HashMap::new();
    mk(SchedulePhase::PreWarned, &mut cycles);
    assert_eq!(
        compute_next_deadline(&cycles),
        Some(datetime!(2026-08-21 12:25 UTC))
    );
    let mut cycles = std::collections::HashMap::new();
    mk(SchedulePhase::FinalWarned, &mut cycles);
    assert_eq!(compute_next_deadline(&cycles), Some(end));
    let mut cycles = std::collections::HashMap::new();
    mk(SchedulePhase::OffDuty, &mut cycles);
    assert_eq!(
        compute_next_deadline(&cycles),
        Some(datetime!(2026-08-21 13:30 UTC))
    );
}

#[test]
fn next_deadline_is_min_across_cycles() {
    let mut cycles = std::collections::HashMap::new();
    let (k1, cs1) = working_cycle("ws1", datetime!(2026-08-21 12:30 UTC));
    cycles.insert(k1, cs1);
    let (k2, cs2) = working_cycle("ws2", datetime!(2026-08-21 15:00 UTC));
    cycles.insert(k2, cs2);
    assert_eq!(
        compute_next_deadline(&cycles),
        Some(datetime!(2026-08-21 12:20 UTC))
    );
}
