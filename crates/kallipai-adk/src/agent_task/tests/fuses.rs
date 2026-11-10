//! The timers as guards: a permit that lost its race is inert, a re-arm replaces
//! the deadline, and a pacing wake sits where the earliest window ends.

use super::super::fuses::{
    arm_pacing_wake, arm_wait_timer, clear_pacing_wake, clear_transient_retry, clear_wait_timer,
    pacing_wake_due, schedule_transient_retry, transient_retry_due, wait_timer_due,
};
use kallipai_common::timefmt;
use std::sync::Arc;

/// Guard-authority pin: a stored wait permit is inert once the timer was
/// cleared (an external event won the race) — `wait_timer_due` is the
/// wait arm's sole authority, so the stale permit cannot fire a turn.
#[tokio::test]
async fn stale_wait_permit_is_inert_after_clear() {
    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    arm_wait_timer(&mut ctx, 600);
    assert!(!wait_timer_due(&ctx), "freshly armed fuse is not yet due");
    ctx.wait_notify.notify_one(); // stale permit: timer fires after the clear
    clear_wait_timer(&ctx);
    assert!(!wait_timer_due(&ctx), "stale permit must be inert");
}

/// Re-arming replaces the fuse: the second `break(wait)` wins (latest
/// deadline + its own armed-secs), mirroring the transient-retry
/// overwrite semantics.
#[tokio::test]
async fn rearming_wait_replaces_deadline_and_secs() {
    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    arm_wait_timer(&mut ctx, 600);
    let first = ctx.wait_until.lock().unwrap().unwrap();
    arm_wait_timer(&mut ctx, 0); // zero-delay fuse: due immediately
    let second = ctx.wait_until.lock().unwrap().unwrap();
    assert!(
        second < first,
        "re-arm must replace, not min/max, the deadline"
    );
    assert_eq!(ctx.wait_armed_secs, 0);
    assert!(wait_timer_due(&ctx));
}

/// Guard-authority twin for the retry arm (the wait twin is above): a
/// stored permit is inert once the retry deadline was cleared — a sleep
/// that fired after an external wake won the race cannot fire a turn.
/// Together with the wait twin this pins both directions of the
/// silent-retry-loss suspicion: permits are doorbells only,
/// the deadline check is the sole authority.
#[tokio::test]
async fn stale_retry_permit_is_inert_after_clear() {
    let mut ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    ctx.transient_fails = 1;
    let (_deadline, _) = schedule_transient_retry(&ctx);
    assert!(
        !transient_retry_due(&ctx),
        "freshly armed backoff is not yet due"
    );
    ctx.retry_notify.notify_one(); // stale permit: sleep fired after the clear
    clear_transient_retry(&ctx);
    assert!(!transient_retry_due(&ctx), "stale permit must be inert");
}

/// The pacing arm's due guard is the authority: a permit a sleep stored after
/// the wake was cleared cannot start a round nobody asked for.
#[tokio::test]
async fn stale_pacing_permit_is_inert_after_clear() {
    let ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    let _ = arm_pacing_wake(&ctx, "test");
    assert!(
        !pacing_wake_due(&ctx),
        "a freshly armed wake is not yet due"
    );
    ctx.pacing_notify.notify_one(); // stale permit: the sleep fired after the clear
    clear_pacing_wake(&ctx);
    assert!(!pacing_wake_due(&ctx), "a stale permit must be inert");
}

/// The wake is armed at the window's end, never before it: a fact the table
/// holds for this profile sets the alarm, and a profile with no readable fact
/// still gets one — from the kind's own interval counted at now, which only
/// ever sets the alarm.
#[tokio::test]
async fn a_pacing_wake_sits_where_the_window_ends() {
    use crate::test_support::{RecordingPacing, make_ctx, profile};
    use kallipai_common::facts::{Block, BlockKind};

    let now = timefmt::now_epoch();
    let pacing = Arc::new(RecordingPacing::default());
    pacing.seed(
        "test",
        Block {
            kind: BlockKind::Quota,
            until: Some(now + 5),
            detail: "HTTP 429".into(),
            status: Some(429),
            recorded_at: now,
        },
    );
    let mut ctx = make_ctx(vec![profile("test", "ep1", 4096)], &["ep1"]).await;
    ctx.profile_pacing = Some(pacing);

    let armed = arm_pacing_wake(&ctx, "test");
    assert!(!pacing_wake_due(&ctx), "a running window is not a wake");
    let remaining = armed.saturating_duration_since(tokio::time::Instant::now());
    assert!(
        remaining >= std::time::Duration::from_secs(4),
        "the wake may not fire before the window ends: {remaining:?}"
    );

    // No readable fact: the wake still arms, so a park is never a dead end
    // waiting on an outside trigger that may never come.
    let bare = make_ctx(vec![profile("test", "ep1", 4096)], &["ep1"]).await;
    let fallback = arm_pacing_wake(&bare, "test");
    let remaining = fallback.saturating_duration_since(tokio::time::Instant::now());
    assert!(
        remaining > std::time::Duration::from_secs(1),
        "the fallback is a real wait, not an immediate re-run: {remaining:?}"
    );
}

/// The alarm answers to the chain, not to the one profile the round stopped
/// on: with every profile inside a window, the shortest one sets the wake, so
/// a candidate whose window opens sooner is never slept through.
#[tokio::test]
async fn a_pacing_wake_sits_where_the_earliest_window_ends() {
    use crate::test_support::{RecordingPacing, make_ctx, profile};
    use kallipai_common::facts::{Block, BlockKind};

    let now = timefmt::now_epoch();
    let pacing = Arc::new(RecordingPacing::default());
    let window = |until: u64| Block {
        kind: BlockKind::Quota,
        until: Some(until),
        detail: "HTTP 429".into(),
        status: Some(429),
        recorded_at: now,
    };
    pacing.seed("slow", window(now + 600));
    pacing.seed("quick", window(now + 5));
    let mut ctx = make_ctx(
        vec![profile("slow", "ep1", 4096), profile("quick", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    ctx.profile_pacing = Some(pacing);

    let armed = arm_pacing_wake(&ctx, "slow");
    let remaining = armed.saturating_duration_since(tokio::time::Instant::now());
    assert!(
        remaining < std::time::Duration::from_secs(60),
        "the wake answers to the shorter window on the chain: {remaining:?}"
    );
    assert!(
        remaining >= std::time::Duration::from_secs(4),
        "and never fires before the window it waits out ends: {remaining:?}"
    );
}
