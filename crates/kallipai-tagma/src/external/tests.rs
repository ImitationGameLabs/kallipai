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
    projector
        .record_inbound(Some(user.clone()), "hi".into(), None)
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
    projector.record_inbound(None, "op-msg".into(), None).await;
    // Relay inbound -> this peer's partition, stamped.
    projector
        .record_inbound(Some(user.clone()), "user-msg".into(), None)
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
        .record_inbound(Some(user.clone()), "hi".into(), None)
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
        chat_history::append(&db, Some(&peer), Some("handle"), "outbound", "hi", None)
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
