use super::*;
use crate::state::RegistryEntry;
use crate::test_helpers::{make_entry, make_state};
use kallipai_archeion_common::ids::{TagmaId, UserId};
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::AuthoredEvent;
use std::sync::Arc;
use tempfile::TempDir;

fn tagma_id() -> TagmaId {
    TagmaId::from("t".to_string())
}

fn user_sender() -> Participant {
    Participant {
        id: ParticipantId::for_user(&UserId::from("u".to_string())),
        kind: ParticipantKind::Human,
        handle: "Alice".into(),
        tagma_id: None,
    }
}

/// `record_outbound` publishes a stamped `AssistantContent` frame carrying
/// the agent sender and (when a store is present) persists exactly one
/// outbound row under the conversation id.
#[tokio::test]
async fn record_outbound_persists_and_publishes() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let mut rx = state.bus.subscribe::<AuthoredFrame>().unwrap();
    projector.record_outbound("hello".into()).await.unwrap();

    let AuthoredFrame { sender, reply } = rx.recv().await.unwrap();
    assert!(
        sender.kind == ParticipantKind::Agent
            && sender.handle == "Tagma"
            && sender.id == ParticipantId::for_tagma(&TagmaId::from("t".to_string())),
        "agent sender stamped: {sender:?}"
    );
    let id = match &reply {
        TagmaReply::Event {
            history_id, event, ..
        } => {
            assert!(history_id > &0, "row id stamped");
            assert!(
                matches!(event, AuthoredEvent::AssistantContent { content } if content == "hello")
            );
            *history_id
        }
        other => panic!("expected Event, got {other:?}"),
    };
    // Exactly one outbound row persisted. With no prior inbound the turn's
    // partition is the operator (NULL), so read the NULL partition.
    let rows = chat_history::read_last_n(&db, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].direction, "outbound");
    assert_eq!(rows[0].text, "hello");
}

/// `record_inbound` publishes a stamped `UserMessage` frame carrying the
/// peer sender and persists one inbound row under that peer's partition, so
/// a serving path can promote the optimistic line and a history pull can
/// replay it.
#[tokio::test]
async fn record_inbound_persists_and_publishes() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let mut rx = state.bus.subscribe::<AuthoredFrame>().unwrap();
    let user = user_sender();
    let returned = projector
        .record_inbound(Some(user.clone()), "hi".into(), None, None)
        .await;

    let AuthoredFrame { sender, reply } = rx.recv().await.unwrap();
    assert!(sender.kind == ParticipantKind::Human);
    let id = match reply {
        TagmaReply::UserMessage {
            history_id, text, ..
        } => {
            assert!(history_id > 0, "row id stamped");
            assert_eq!(text, "hi");
            history_id
        }
        other => panic!("expected UserMessage, got {other:?}"),
    };
    assert_eq!(returned, Some(id), "the caller gets the row it wrote");
    let rows = chat_history::read_last_n(&db, Some(user.id.as_ref()), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].direction, "inbound");

    // The history read (filtered to this peer) decodes the inbound row back
    // into a UserMessage paired with the peer sender.
    let page = projector
        .read_history(Some(user.id.as_ref()), None, None, 50)
        .await
        .expect("a readable store answers");
    assert!(!page.more);
    assert_eq!(page.entries.len(), 1);
    match &page.entries[0] {
        HistoryEntry {
            sender,
            reply: TagmaReply::UserMessage {
                history_id, text, ..
            },
        } => {
            assert_eq!(sender.kind, ParticipantKind::Human);
            assert_eq!(sender.id, user.id);
            assert_eq!(*history_id, id);
            assert_eq!(text, "hi");
        }
        other => panic!("expected user UserMessage on replay, got {other:?}"),
    }

    // A quoting inbound: the published frame carries the id, so a reader that
    // learns the row from the wire (the cache write, a replay, a second
    // client) can draw its block; the replay carries it too.
    projector
        .record_inbound(Some(user.clone()), "about that".into(), None, Some(id))
        .await;
    let quoted = match rx.recv().await.unwrap().reply {
        TagmaReply::UserMessage { quote, .. } => quote,
        other => panic!("expected UserMessage, got {other:?}"),
    };
    assert_eq!(quoted, Some(id), "the frame carries the quoted row id");
    let page = projector
        .read_history(Some(user.id.as_ref()), None, None, 50)
        .await
        .expect("a readable store answers");
    assert_eq!(page.entries.len(), 2);
    match &page.entries[1].reply {
        TagmaReply::UserMessage { quote, .. } => {
            assert_eq!(*quote, Some(id), "the replayed row carries it too")
        }
        other => panic!("expected UserMessage on replay, got {other:?}"),
    }
}

/// Without a store (never-enrolled tagma) the projector still forwards live
/// frames (unstamped) so the direct SSE path keeps working local-only.
#[tokio::test]
async fn record_outbound_works_without_store() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        None,
        None,
        Some(tagma_id()),
        None,
        MessageLimits::default(),
    );
    let mut rx = state.bus.subscribe::<AuthoredFrame>().unwrap();
    projector.record_outbound("hello".into()).await.unwrap();
    match rx.recv().await.unwrap() {
        AuthoredFrame {
            reply: TagmaReply::Event { history_id, .. },
            ..
        } => {
            assert_eq!(history_id, 0, "no store -> unstamped");
        }
        other => panic!("expected unstamped Event, got {other:?}"),
    }
}

/// Direct vs relay partitions: a direct inbound (`None`) lands in the
/// `NULL` (operator) partition and stamps a row; a relay inbound (`Some`)
/// lands in that peer's partition. Each `read_history` sees only its own
/// partition. There is no persist gate — rows are always written when the
/// store is present.
#[tokio::test]
async fn direct_and_relay_partitions_persist_and_filter() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        None,
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let user = user_sender();

    // Direct inbound -> operator (NULL) partition, stamped.
    projector
        .record_inbound(None, "op-msg".into(), None, None)
        .await;
    // Relay inbound -> this peer's partition, stamped.
    projector
        .record_inbound(Some(user.clone()), "user-msg".into(), None, None)
        .await;

    // read_history(None) sees only the operator partition; the relay row is
    // in a different partition and is excluded.
    let op_page = projector
        .read_history(None, None, None, 50)
        .await
        .expect("a readable store answers");
    assert_eq!(op_page.entries.len(), 1);
    match &op_page.entries[0] {
        HistoryEntry {
            sender,
            reply: TagmaReply::UserMessage { text, .. },
        } => {
            // NULL rows resolve to the operator sender on the wire.
            assert_eq!(sender.kind, ParticipantKind::Human);
            assert_eq!(text, "op-msg");
        }
        other => panic!("expected operator UserMessage, got {other:?}"),
    }
    // read_history(Some(peer)) sees only that peer's partition.
    let user_page = projector
        .read_history(Some(user.id.as_ref()), None, None, 50)
        .await
        .expect("a readable store answers");
    assert_eq!(user_page.entries.len(), 1);
    match &user_page.entries[0] {
        HistoryEntry {
            sender,
            reply: TagmaReply::UserMessage { text, .. },
        } => {
            assert_eq!(sender.id, user.id);
            assert_eq!(text, "user-msg");
        }
        other => panic!("expected peer UserMessage, got {other:?}"),
    }
}

/// A fresh partition's first outbound message is one unread: the store seeds
/// the watermark BEFORE it appends, so the new row sits above it. Seeded after
/// the append, that row would be the watermark and the badge would open at
/// zero.
#[tokio::test]
async fn first_outbound_message_of_a_fresh_partition_is_unread() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let mut unread_rx = state.bus.subscribe::<UnreadFrame>().unwrap();
    let user = user_sender();
    projector
        .record_inbound(Some(user.clone()), "hi".into(), None, None)
        .await;
    let peer = user.id.as_ref();
    let inbound_id = chat_history::read_last_n(&db, Some(peer), 10)
        .await
        .unwrap()[0]
        .id;
    projector.record_outbound("hello".into()).await.unwrap();

    let UnreadFrame(snapshot) = unread_rx.recv().await.unwrap();
    assert_eq!(snapshot.peer.as_str(), peer);
    assert_eq!(
        snapshot.cursor, inbound_id,
        "watermark sits on the inbound row"
    );
    assert_eq!(snapshot.unread, 1, "the reply above it is unread");
}

/// The cold-start warm holds one snapshot per known partition, however many
/// there are. The flusher takes that set straight into its pending buffer, so
/// the unread topic's ring (16 slots) never bounds it -- routed through the
/// bus, an install with more partitions would lose the tail before the first
/// drain.
#[tokio::test]
async fn cold_start_covers_every_partition_beyond_the_bus_ring() {
    let state = make_state();
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        None,
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    const PARTITIONS: usize = 20;
    for i in 0..PARTITIONS {
        let peer = format!("peer-{i}");
        chat_history::append(
            &db,
            Some(&peer),
            Some("handle"),
            "outbound",
            "hi",
            None,
            None,
        )
        .await
        .unwrap();
    }
    let mut peers: Vec<String> = projector
        .cold_start_unread()
        .await
        .into_iter()
        .map(|s| s.peer)
        .collect();
    peers.sort();
    let mut expected: Vec<String> = (0..PARTITIONS).map(|i| format!("peer-{i}")).collect();
    expected.sort();
    assert_eq!(peers, expected);
}

/// A store the read cannot answer from is a failed read, not an empty
/// conversation: the projector reports it instead of an empty page.
#[tokio::test]
async fn read_history_reports_an_unreadable_store() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    sea_orm::ConnectionTrait::execute(
        &db,
        sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "DROP TABLE chat_history".to_owned(),
        ),
    )
    .await
    .expect("break the store for the duration of the test");

    projector
        .read_history(None, None, None, 50)
        .await
        .expect_err("an unreadable store is not an empty history");
}

/// A row the decoder cannot read is skipped on the wire but counted in the
/// page, so the caller can tell "some of this window is missing" from "this
/// window is complete".
#[tokio::test]
async fn read_history_counts_the_rows_it_cannot_decode() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "tok".into())),
            )
            .unwrap();
    }
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let user = user_sender();
    for (direction, text) in [("inbound", "good"), ("sideways", "corrupt")] {
        chat_history::append(
            &db,
            Some(user.id.as_ref()),
            Some(user.handle.as_str()),
            direction,
            text,
            None,
            None,
        )
        .await
        .unwrap();
    }

    let page = projector
        .read_history(Some(user.id.as_ref()), None, None, 50)
        .await
        .expect("a readable store answers");
    assert_eq!(page.entries.len(), 1, "the decodable row still replays");
    assert_eq!(page.undecodable, 1, "the undecodable row is counted");
}

/// A query naming two cursors asks for two windows at once, so it is
/// refused even when the projector carries no store: a missing store is
/// not an answer to a question that has no answer.
#[tokio::test]
async fn read_history_refuses_two_cursors_without_a_store() {
    let state = make_state();
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        None,
        None,
        Some(tagma_id()),
        None,
        MessageLimits::default(),
    );
    let err = projector
        .read_history(
            None,
            Some(HistoryCursor::Position(10)),
            Some(HistoryCursor::Position(20)),
            50,
        )
        .await
        .expect_err("two cursors name two windows");
    assert!(
        matches!(
            err,
            HistoryReadError::BothCursors {
                after: 10,
                before: 20
            }
        ),
        "expected BothCursors, got {err:?}"
    );
}

/// A limit of zero asks for a window that cannot hold anything: refused rather
/// than answered with an empty page the caller would read as an empty
/// conversation.
#[tokio::test]
async fn read_history_refuses_a_zero_limit() {
    let state = make_state();
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let err = projector
        .read_history(None, None, None, 0)
        .await
        .expect_err("a zero-row window has no answer");
    assert!(
        matches!(err, HistoryReadError::ZeroLimit { limit: 0 }),
        "expected ZeroLimit, got {err:?}"
    );
}

/// `broadcast::channel(1)` refuses a send that has no live receiver, so a
/// successful send is proof the pump has subscribed to the root's broadcast.
async fn send_when_subscribed(tx: &broadcast::Sender<SseEvent>, text: &str) {
    for _ in 0..200 {
        if tx
            .send(SseEvent::AssistantContent {
                content: text.into(),
            })
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the pump never subscribed to the root's broadcast");
}

async fn expect_authored(frames: &mut crate::bus::TopicReceiver<AuthoredFrame>) -> AuthoredFrame {
    tokio::time::timeout(Duration::from_secs(10), frames.recv())
        .await
        .expect("no authored frame reached the bus within 10s")
        .expect("the bus receiver closed")
}

/// A root restart looks like `RecvError::Closed` to the pump: the old entry
/// (and every sender it held) is dropped, and a new live root appears later.
/// The pump must wait for it rather than end.
#[tokio::test]
async fn pump_survives_root_restart() {
    let state = make_state();
    let root = AgentId::from("root".to_string());
    let entry = make_entry(None, "tok".into());
    let tx1 = entry.agent.events_tx.clone();
    state
        .registry
        .write()
        .await
        .register_root(root.clone(), RegistryEntry::Live(entry))
        .unwrap();
    // No history store: `stamp` degrades to a no-op and the frame publishes
    // anyway, which is all this test needs.
    let _projector = ExternalProjector::new(
        Arc::downgrade(&state),
        None,
        Some(ConversationId::for_tagma(&tagma_id())),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let mut frames = state.bus.subscribe::<AuthoredFrame>().unwrap();
    send_when_subscribed(&tx1, "first").await;
    let _ = expect_authored(&mut frames).await;

    // Drop our sender clone too, or the channel never reports `Closed`.
    drop(tx1);
    assert!(state.registry.write().await.unregister(&root).is_some());
    // Outlive the re-subscribe attempt made 2s after `Closed`: with no root
    // to subscribe to, a pump that gives up here is already gone.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let entry = make_entry(None, "tok".into());
    let tx2 = entry.agent.events_tx.clone();
    state
        .registry
        .write()
        .await
        .register_root(root.clone(), RegistryEntry::Live(entry))
        .unwrap();
    send_when_subscribed(&tx2, "second").await;
    let second = expect_authored(&mut frames).await;
    assert!(matches!(second.reply, TagmaReply::Event { .. }));
}

/// Boot with no root at all: the pump waits, and picks the root up the moment
/// it is registered — the other half of "the root may be transiently absent".
#[tokio::test]
async fn pump_waits_for_a_root_that_never_existed() {
    let state = make_state();
    let _projector = ExternalProjector::new(
        Arc::downgrade(&state),
        None,
        Some(ConversationId::for_tagma(&tagma_id())),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let mut frames = state.bus.subscribe::<AuthoredFrame>().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let entry = make_entry(None, "tok".into());
    let tx = entry.agent.events_tx.clone();
    state
        .registry
        .write()
        .await
        .register_root(
            AgentId::from("root".to_string()),
            RegistryEntry::Live(entry),
        )
        .unwrap();
    send_when_subscribed(&tx, "late").await;
    let _ = expect_authored(&mut frames).await;
}

/// Cancel is one of the pump's two exits; it must take it promptly.
#[tokio::test]
async fn pump_exits_on_cancel() {
    let state = make_state();
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        None,
        None,
        None,
        None,
        MessageLimits::default(),
    );
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(projector.clone().run_event_pump(cancel.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the pump did not exit on cancel")
        .expect("the pump task panicked");
}

/// The other exit: the world goes away (the last `Arc<AppState>` drops) while
/// the pump is still waiting for a root. It must end, not spin forever.
#[tokio::test]
async fn pump_exits_when_world_gone() {
    let cancel = CancellationToken::new();
    let state = make_state();
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        None,
        None,
        None,
        None,
        MessageLimits::default(),
    );
    let handle = tokio::spawn(projector.clone().run_event_pump(cancel.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(state);
    tokio::time::timeout(Duration::from_secs(6), handle)
        .await
        .expect("the pump did not exit when the world went away")
        .expect("the pump task panicked");
}

/// A position page answers where the same request continues, and `more` is an
/// existence question rather than a fullness one: a page that filled to `limit`
/// with nothing behind it says false, and a short page with rows behind it says
/// true.
#[tokio::test]
async fn a_position_page_names_its_successor_and_stops_at_the_end() {
    let state = make_state();
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    for text in ["one", "two", "three"] {
        chat_history::append(&db, None, None, "inbound", text, None, None)
            .await
            .unwrap();
    }

    // The recent snapshot: each row names its own position, and the newest
    // position is where a reader resumes from.
    let page = projector.read_history(None, None, None, 10).await.unwrap();
    let seqs: Vec<i64> = page
        .entries
        .iter()
        .map(|e| match &e.reply {
            TagmaReply::UserMessage { seq, .. } => seq.expect("a stored row names its position"),
            other => panic!("expected UserMessage, got {other:?}"),
        })
        .collect();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(page.next, Some(3));
    assert!(!page.more);

    // A catch-up page that filled to the limit with nothing behind it: more is
    // false, which is what lets a resuming reader stop instead of asking again.
    let page = projector
        .read_history(None, Some(HistoryCursor::Position(0)), None, 3)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 3);
    assert_eq!(page.next, Some(3));
    assert!(!page.more, "three of three rows, nothing behind them");

    // A short page with a row behind it: more is true, so the reader asks again
    // from `next` rather than assuming it is caught up.
    let page = projector
        .read_history(None, Some(HistoryCursor::Position(0)), None, 2)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.next, Some(2));
    assert!(page.more);

    // An empty catch-up page echoes the cursor back, so a reader that loops on
    // `next` cannot fall back to the start of the conversation.
    let page = projector
        .read_history(None, Some(HistoryCursor::Position(3)), None, 10)
        .await
        .unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.next, Some(3));
    assert!(!page.more);

    // Scroll-up walks backwards: the continuation is the page's oldest row.
    let page = projector
        .read_history(None, None, Some(HistoryCursor::Position(3)), 2)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.next, Some(1));
    assert!(!page.more, "nothing older than the first row");
}

/// A row the decoder cannot read still occupies a position: the page reports it
/// in `undecodable` and `next` moves past it, so a reader neither re-reads the
/// same block forever nor skips whatever sits behind the block.
#[tokio::test]
async fn a_page_of_undecodable_rows_still_says_where_to_go() {
    let state = make_state();
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(
        &dir.path().join("h.sqlite"),
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let conv = ConversationId::for_tagma(&tagma_id());
    let projector = ExternalProjector::new(
        Arc::downgrade(&state),
        Some(db.clone()),
        Some(conv.clone()),
        Some(tagma_id()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    for direction in ["inbound", "sideways", "inbound"] {
        chat_history::append(&db, None, None, direction, "text", None, None)
            .await
            .unwrap();
    }

    let page = projector
        .read_history(None, Some(HistoryCursor::Position(1)), None, 10)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1, "the readable row behind the hole");
    assert_eq!(page.undecodable, 1);
    assert_eq!(
        page.next,
        Some(3),
        "the undecodable row still advances the cursor"
    );
    assert!(!page.more);
}
