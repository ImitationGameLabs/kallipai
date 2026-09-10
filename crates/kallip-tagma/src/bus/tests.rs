use super::*;

use kallip_archeion_common::ids::{ParticipantId, ParticipantKind, UserId};
use kallip_common::protocol::AgentState;
use kallip_common::protocol::AuthoredEvent;
use kallip_lesche_common::event::TagmaStatusPayload;
use kallip_lesche_common::projection::ProjectionSnapshot as ProjectionSnapshotPayload;
use std::time::Duration;

// Local vocabulary for the generic-core legs: the core must work for any
// `Event` impl, so these legs exercise it with their own types and leave
// the real vocabulary to the roundtrip leg.
#[derive(Clone)]
struct Ping(u32);
impl Event for Ping {
    const TOPIC: &'static str = "test_ping";
}
#[derive(Clone)]
struct Pong;
impl Event for Pong {
    const TOPIC: &'static str = "test_pong";
}
#[derive(Clone)]
struct Unregistered;
impl Event for Unregistered {
    const TOPIC: &'static str = "test_unregistered";
}

fn test_bus() -> EventBus {
    EventBus::builder()
        .topic::<Ping>(4)
        .topic::<Pong>(4)
        .build()
        .expect("distinct test topics")
}

fn user_sender() -> Participant {
    Participant {
        id: ParticipantId::for_user(&UserId::from("u".to_string())),
        kind: ParticipantKind::Human,
        handle: "Alice".into(),
        tagma_id: None,
    }
}

/// Publish → subscribe roundtrip over the real tagma vocabulary: typed
/// payloads survive the erased slot (the single downcast) intact and in
/// order, and both topics' publishes counters land at the single
/// instrumentation site.
#[tokio::test]
async fn tagma_topics_roundtrip_preserve_payloads() {
    let bus = tagma_bus().expect("static registry is conflict-free");
    let mut authored = bus.subscribe::<AuthoredFrame>().unwrap();
    let mut signals = bus.subscribe::<SignalFrame>().unwrap();
    let mut statuses = bus.subscribe::<StatusSnapshot>().unwrap();
    let mut projections = bus.subscribe::<ProjectionSnapshot>().unwrap();
    let mut tasks = bus.subscribe::<TaskChanged>().unwrap();
    bus.publish(AuthoredFrame {
        sender: user_sender(),
        reply: TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hello".into(),
            },
            history_id: 7,
            created_at: None,
        },
    })
    .unwrap();
    bus.publish(SignalFrame(SignalEvent::Busy)).unwrap();
    let status = TagmaStatusPayload {
        root_state: AgentState::Idle,
        subagents_total: 2,
        subagents_active: 1,
        token_budget: 50_000,
        token_consumed: 1_234,
        token_budget_unlimited: false,
    };
    bus.publish(StatusSnapshot(status.clone())).unwrap();
    bus.publish(ProjectionSnapshot(ProjectionSnapshotPayload {
        agents: Vec::new(),
        status: status.clone(),
        push_seq: 9,
        work_schedule: None,
    }))
    .unwrap();
    bus.publish(TaskChanged {
        task_id: 5,
        title: "ledger walk".into(),
        status: "in_progress".into(),
        verb: "start".into(),
        creator: Some("root".into()),
        assignee: Some("scout".into()),
        seats: vec!["scout".into()],
    })
    .unwrap();

    let got = authored.recv().await.unwrap();
    assert_eq!(got.sender.handle, "Alice");
    let history_id = match got.reply {
        TagmaReply::Event {
            event, history_id, ..
        } => {
            assert!(
                matches!(event, AuthoredEvent::AssistantContent { content } if content == "hello"),
                "authored content survives the slot"
            );
            history_id
        }
        other => panic!("expected Event reply, got {other:?}"),
    };
    assert_eq!(history_id, 7);
    assert!(matches!(signals.recv().await.unwrap().0, SignalEvent::Busy));
    assert_eq!(statuses.recv().await.unwrap().0, status);
    let projection = projections.recv().await.unwrap().0;
    assert_eq!(projection.push_seq, 9, "the payload survives the slot");
    let task = tasks.recv().await.unwrap();
    assert_eq!(
        (task.task_id, task.title.as_str(), task.status.as_str()),
        (5, "ledger walk", "in_progress"),
        "the task payload survives the slot"
    );
    let stats = bus.stats();
    assert_eq!(
        stats,
        vec![
            TopicStats {
                topic: "authored_frame",
                publishes: 1,
                lagged: 0,
                last_gap: None
            },
            TopicStats {
                topic: "signal_frame",
                publishes: 1,
                lagged: 0,
                last_gap: None
            },
            TopicStats {
                topic: "status_snapshot",
                publishes: 1,
                lagged: 0,
                last_gap: None
            },
            TopicStats {
                topic: "projection_snapshot",
                publishes: 1,
                lagged: 0,
                last_gap: None
            },
            TopicStats {
                topic: "task_changed",
                publishes: 1,
                lagged: 0,
                last_gap: None
            },
        ]
    );
}

/// A duplicate event type is rejected at build: two topics sharing one
/// TypeId key is a registration-site programming error, and the registry
/// (not the call sites) is the idempotence guard.
#[test]
fn duplicate_type_is_rejected_at_build() {
    let result = EventBus::builder()
        .topic::<Ping>(4)
        .topic::<Ping>(8)
        .build();
    assert!(matches!(result, Err(RegistryError::Duplicate { .. })));
}

/// Publish/subscribe on an unregistered topic is a typed error, not a
/// panic — the hot-path caller logs and drops (the no-op leg)
/// — and leaves the registered topics untouched.
#[test]
fn unregistered_topic_errors_without_side_effects() {
    let bus = test_bus();
    assert!(matches!(
        bus.publish(Unregistered),
        Err(PublishError::Unregistered { .. })
    ));
    assert!(matches!(
        bus.subscribe::<Unregistered>(),
        Err(SubscribeError::Unregistered { .. })
    ));
    assert!(
        bus.stats()
            .iter()
            .all(|s| s.publishes == 0 && s.lagged == 0)
    );
}

/// A send with no receivers is benign Ok — live-only topics need no
/// durable echo — and still counts as a publish.
#[test]
fn publish_without_subscribers_is_ok_and_counted() {
    let bus = test_bus();
    bus.publish(Ping(1)).unwrap();
    let ping = bus
        .stats()
        .into_iter()
        .find(|s| s.topic == "test_ping")
        .unwrap();
    assert_eq!(ping.publishes, 1);
    // Unobserved sends still spend seqs (allocation-point semantics):
    // two more no-subscriber publishes make the first post-subscribe
    // event carry the fourth seq — benign by bus contract.
    for i in 1..3 {
        bus.publish(Ping(i)).unwrap();
    }
    let mut rx = bus.subscribe::<Ping>().unwrap();
    let allocated = bus.publish(Ping(3)).unwrap();
    let Sequenced { seq, event } = rx.rx.try_recv().unwrap();
    assert_eq!(allocated, seq, "publish returns the seq it hands out");
    assert_eq!(
        seq, 3,
        "seqs spent unobserved are skipped, not replayed (4th seq)"
    );
    assert_eq!(event.0, 3, "payload integrity across spent seqs");
}

/// A receiver that falls past the topic capacity gets `Lagged(n)` on its
/// next recv and the topic's lagged counter records the loss (the lag
/// counter leg); the surviving tail still arrives in order. (The channel
/// itself stays open here: the bus — its sender — outlives the receiver
/// in this test, so closure is asserted via `try_recv` = `Empty`.)
#[tokio::test]
async fn lag_is_reported_and_counted() {
    let bus = test_bus();
    let mut rx = bus.subscribe::<Ping>().unwrap();
    for i in 0..6 {
        bus.publish(Ping(i)).unwrap();
    }
    assert!(matches!(rx.recv().await, Err(RecvError::Lagged(2))));
    let ping = bus
        .stats()
        .into_iter()
        .find(|s| s.topic == "test_ping")
        .unwrap();
    assert_eq!(ping.lagged, 2);
    // First lag before any delivery: the count is exact, the range is
    // not knowable — `last_delivered` has no anchor yet (honest
    // degradation).
    assert_eq!(ping.last_gap, None);
    for i in 2..6 {
        assert_eq!(rx.recv().await.unwrap().0, i);
    }
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    // A second lag with a prior delivery anchors the interval: ten more
    // events through the cap-4 ring lose seqs 6..=11 —
    // [last+1, last+n] = [6, 11] lands in stats.
    for i in 6..16 {
        bus.publish(Ping(i)).unwrap();
    }
    assert!(matches!(rx.recv().await, Err(RecvError::Lagged(6))));
    let ping = bus
        .stats()
        .into_iter()
        .find(|s| s.topic == "test_ping")
        .unwrap();
    assert_eq!(ping.lagged, 8);
    assert_eq!(ping.last_gap, Some((6, 11)));
    for i in 12..16 {
        assert_eq!(rx.recv().await.unwrap().0, i);
    }
}

/// The dispatch cost — one index lookup plus one downcast per publish,
/// never per receiver (the mid-envelope form) — is the
/// measured-upgrade evidence. The leg prints the per-publish figure
/// for the report and asserts only a pathological ceiling, so it cannot
/// flake on a loaded runner.
#[tokio::test]
async fn dispatch_cost_stays_microscopic() {
    let bus = test_bus();
    let mut rx = bus.subscribe::<Ping>().unwrap();
    const N: u32 = 10_000;
    let start = std::time::Instant::now();
    for i in 0..N {
        bus.publish(Ping(i)).unwrap();
    }
    let elapsed = start.elapsed();
    let per_publish = elapsed.as_nanos() / u128::from(N);
    eprintln!("bus dispatch: {per_publish} ns/publish across {N} publishes");
    assert!(
        elapsed < Duration::from_secs(1),
        "dispatch pathologically slow: {elapsed:?} for {N} publishes"
    );
    while rx.try_recv().is_ok() {}
}

/// The ordering-lock leg: threads publishing one topic concurrently
/// must land in-channel order == allocation order — the precondition
/// downstream gap rules rely on. The lock holds across
/// seq allocation and send, so the channel order is the global
/// allocation order regardless of thread scheduling; the assertion
/// checks the raw arrival sequence against 0..total, so dropping the
/// lock (atomic-only fallback) is detectable across runs.
#[test]
fn seq_monotonic_under_concurrent_publishers() {
    let bus = Arc::new(
        EventBus::builder()
            .topic::<Ping>(1024)
            .build()
            .expect("single test topic"),
    );
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 25;
    let mut rx = bus.subscribe::<Ping>().unwrap();
    // Subscribe BEFORE spawning: a broadcast receiver only sees sends
    // past its subscription point, so subscribing after the spawn would
    // silently skip the earliest seqs (schedule-dependent — the bug
    // this leg exists to guard against).
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let bus = Arc::clone(&bus);
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                bus.publish(Ping(i as u32)).unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let total = (THREADS * PER_THREAD) as usize;
    let mut seqs = Vec::with_capacity(total);
    for _ in 0..total {
        let Sequenced { seq, .. } = rx.rx.try_recv().unwrap();
        seqs.push(seq);
    }
    let expected: Vec<u64> = (0..total as u64).collect();
    assert_eq!(
        seqs, expected,
        "in-channel order equals allocation order across 0..total"
    );
}
/// The SSE-face stream adapter keeps the counting leg: a consumer draining
/// through `into_stream` has its `Lagged` absorbed as a skip yet still
/// lands in the topic's counter — identical to a `recv` consumer (the
/// merged SSE sources count through this path).
#[tokio::test]
async fn into_stream_skips_lagged_and_counts_the_loss() {
    let bus = EventBus::builder()
        .topic::<Ping>(2)
        .build()
        .expect("single test topic");
    let rx = bus.subscribe::<Ping>().unwrap();
    for i in 0..4 {
        bus.publish(Ping(i)).unwrap();
    }
    let mut stream = rx.into_stream();
    // Ring cap 2, four published: the first poll absorbs Lagged(2)
    // (counted, skipped) and the two surviving frames arrive in order.
    assert!(matches!(stream.next().await, Some(Ping(2))));
    assert!(matches!(stream.next().await, Some(Ping(3))));
    let ping = bus
        .stats()
        .into_iter()
        .find(|s| s.topic == "test_ping")
        .unwrap();
    assert_eq!(ping.lagged, 2);
    assert_eq!(ping.publishes, 4);
    // First lag before any delivery: counted, range unknown (the
    // honest-degraded case — `last_delivered` has no anchor yet).
    assert_eq!(ping.last_gap, None);
}
