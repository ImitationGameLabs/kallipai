use super::*;
use crate::state::RegistryEntry;
use crate::test_helpers::{make_entry, make_state};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_common::agentid::AgentId;
use kallip_common::protocol::AuthoredEvent;
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
    let db = chat_history::open(&dir.path().join("h.sqlite"))
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
    let mut rx = projector.subscribe();
    projector.record_outbound("hello".into()).await.unwrap();

    let (sender, reply) = match rx.recv().await.unwrap() {
        ExternalFrame::Authored { sender, reply } => (sender, reply),
        other => panic!("expected Authored, got {other:?}"),
    };
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
    let db = chat_history::open(&dir.path().join("h.sqlite"))
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
    let mut rx = projector.subscribe();
    let user = user_sender();
    projector
        .record_inbound(Some(user.clone()), "hi".into())
        .await;

    let (sender, reply) = match rx.recv().await.unwrap() {
        ExternalFrame::Authored { sender, reply } => (sender, reply),
        other => panic!("expected Authored, got {other:?}"),
    };
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
    let (entries, more) = projector
        .read_history(Some(user.id.as_ref()), None, None, 50)
        .await;
    assert!(!more);
    assert_eq!(entries.len(), 1);
    match &entries[0] {
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
    let mut rx = projector.subscribe();
    projector.record_outbound("hello".into()).await.unwrap();
    match rx.recv().await.unwrap() {
        ExternalFrame::Authored {
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
    let db = chat_history::open(&dir.path().join("h.sqlite"))
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
    projector.record_inbound(None, "op-msg".into()).await;
    // Relay inbound -> this peer's partition, stamped.
    projector
        .record_inbound(Some(user.clone()), "user-msg".into())
        .await;

    // read_history(None) sees only the operator partition; the relay row is
    // in a different partition and is excluded.
    let (op_entries, _) = projector.read_history(None, None, None, 50).await;
    assert_eq!(op_entries.len(), 1);
    match &op_entries[0] {
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
    let (user_entries, _) = projector
        .read_history(Some(user.id.as_ref()), None, None, 50)
        .await;
    assert_eq!(user_entries.len(), 1);
    match &user_entries[0] {
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
