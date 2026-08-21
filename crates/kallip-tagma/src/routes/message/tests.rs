use super::*;
use crate::auth::{AuthIdentity, Identity};
use crate::state::{AgentId, RegistryEntry};
use crate::test_helpers::{
    add_faulted_root, add_root, add_sub, install_inbox_store, make_entry_with_rx, make_state,
};
use axum::Json;
use axum::extract::{Path, State};
use kallip_common::protocol::MessageRequest;

// -- send_message: sender identity is attached to the delivered payload --

/// Deliver a message as the operator. The full envelope (with header) is
/// stored in the inbox.
#[tokio::test]
async fn operator_message_stores_envelope_in_inbox() {
    let state = make_state();
    install_inbox_store(&state).await;
    let receiver = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(None, "recv".into());
    entry.identity.config.role = "root".into();
    state
        .registry
        .write()
        .await
        .register(receiver.clone(), RegistryEntry::Live(entry));

    let resp = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(receiver.clone()),
        Json(MessageRequest {
            text: "do the thing".into(),
        }),
    )
    .await
    .expect("operator send accepted");
    assert_eq!(resp.0, StatusCode::ACCEPTED);

    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&receiver)
        .await
        .unwrap();
    assert!(msg.contains("[From: operator]"));
    assert!(msg.contains("do the thing"));
}

/// Deliver a message from a child agent to its parent. The inbox stores
/// the full envelope with sender id + role + relation.
#[tokio::test]
async fn agent_message_stores_sender_and_relation() {
    let state = make_state();
    install_inbox_store(&state).await;
    let parent = AgentId::random();
    let child = AgentId::random();

    let (mut parent_entry, _parent_rx) = make_entry_with_rx(None, "parent".into());
    parent_entry.identity.config.role = "lead".into();
    state
        .registry
        .write()
        .await
        .register(parent.clone(), RegistryEntry::Live(parent_entry));

    let (mut child_entry, _child_rx) = make_entry_with_rx(Some(parent.clone()), "child".into());
    child_entry.identity.config.role = "researcher".into();
    state
        .registry
        .write()
        .await
        .register(child.clone(), RegistryEntry::Live(child_entry));

    let resp = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Agent { id: child.clone() }),
        Path(parent.clone()),
        Json(MessageRequest {
            text: "results attached".into(),
        }),
    )
    .await
    .expect("agent send accepted");
    assert_eq!(resp.0, StatusCode::ACCEPTED);

    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&parent)
        .await
        .unwrap();
    assert!(msg.contains(&child.to_string()));
    assert!(msg.contains("researcher"));
    assert!(msg.contains("results attached"));
}

/// Self-message: an agent messaging itself is stored in the inbox.
#[tokio::test]
async fn self_message_stored_in_inbox() {
    let state = make_state();
    install_inbox_store(&state).await;
    let me = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(None, "me".into());
    entry.identity.config.role = "solo".into();
    state
        .registry
        .write()
        .await
        .register(me.clone(), RegistryEntry::Live(entry));

    let _ = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Agent { id: me.clone() }),
        Path(me.clone()),
        Json(MessageRequest {
            text: "note to self".into(),
        }),
    )
    .await
    .expect("self send accepted");

    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&me)
        .await
        .unwrap();
    assert!(msg.contains("note to self"));
}

/// Messaging a faulted agent returns 409 with the reason.
#[tokio::test]
async fn send_message_to_faulted_returns_conflict() {
    let state = make_state();
    install_inbox_store(&state).await;
    let faulted = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_faulted_root(&mut reg, &faulted, "restore failed: missing workspace");
    }
    let err = send_message(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(faulted),
        Json(MessageRequest { text: "hi".into() }),
    )
    .await
    .expect_err("faulted agent rejects messages");
    assert_eq!(err.status, 409);
    assert!(
        err.message.contains("faulted"),
        "message should mention faulted: {}",
        err.message
    );
    assert!(err.message.contains("missing workspace"), "{}", err.message);
}

/// Messaging a parked agent returns 409 naming the wake endpoint — the
/// silent-unreachable UX gap (a buffered message would never wake a parked
/// agent; only the kick does).
#[tokio::test]
async fn send_message_to_parked_returns_conflict_with_wake_hint() {
    let state = make_state();
    install_inbox_store(&state).await;
    let root = AgentId::random();
    let parked = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
        add_sub(&mut reg, &parked, &root);
        let live = reg.get(&parked).unwrap().as_live().unwrap();
        live.agent.state.store(
            crate::state::AgentState::PARKED,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    let err = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(parked),
        Json(MessageRequest { text: "hi".into() }),
    )
    .await
    .expect_err("parked agent rejects ordinary messages");
    assert_eq!(err.status, 409);
    assert!(
        err.message.contains("parked") && err.message.contains("/wake"),
        "message must name the parked state and the wake exit: {}",
        err.message
    );
    // The refusal happens before the inbox push: nothing is buffered.
    assert_eq!(state.inboxes.get().unwrap().len_for(&root).await, 0);
}

// -- Duty gate: off-duty messages buffer to inbox --

/// An off-duty agent buffers messages to its inbox instead of delivering
/// to the prompt channel. The message never reaches `prompt_rx`.
#[tokio::test]
async fn off_duty_message_buffers_to_inbox() {
    let state = make_state();
    install_inbox_store(&state).await;
    let receiver = AgentId::random();
    let (mut entry, mut rx) = make_entry_with_rx(None, "recv".into());
    entry.identity.config.role = "root".into();
    state
        .registry
        .write()
        .await
        .register(receiver.clone(), RegistryEntry::Live(entry));
    // Set the agent off-duty.
    state
        .duty
        .set(receiver.clone(), crate::duty::DutyStatus::OffDuty);

    let resp = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(receiver.clone()),
        Json(MessageRequest {
            text: "urgent task".into(),
        }),
    )
    .await
    .expect("off-duty send should still return accepted");
    assert_eq!(resp.0, StatusCode::ACCEPTED);
    // Warning mentions off-duty buffering.
    assert!(
        resp.1.warning.is_some(),
        "off-duty response should carry a warning"
    );
    assert!(
        resp.1.warning.as_ref().unwrap().contains("off-duty"),
        "warning should mention off-duty: {:?}",
        resp.1.warning
    );
    // Message was NOT delivered to the prompt channel.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .is_err(),
        "no message should be delivered to an off-duty agent"
    );
    // Message IS in the inbox.
    assert_eq!(state.inboxes.get().unwrap().len_for(&receiver).await, 1);
}

/// An on-duty agent receives messages normally (no buffering).
#[tokio::test]
async fn on_duty_message_stored_in_inbox() {
    let state = make_state();
    install_inbox_store(&state).await;
    let receiver = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(None, "recv".into());
    entry.identity.config.role = "root".into();
    state
        .registry
        .write()
        .await
        .register(receiver.clone(), RegistryEntry::Live(entry));

    let resp = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(receiver.clone()),
        Json(MessageRequest {
            text: "hello".into(),
        }),
    )
    .await
    .expect("on-duty send accepted");
    assert_eq!(resp.0, StatusCode::ACCEPTED);
    assert!(resp.1.warning.is_none(), "on-duty should have no warning");

    // On-duty message is stored in the inbox.
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&receiver)
        .await
        .unwrap();
    assert!(msg.contains("hello"));
}

/// Off-duty messages buffer to inbox; on-duty messages also go to inbox.
/// The agent task loop pulls on notify wake.
#[tokio::test]
async fn duty_toggle_off_then_on() {
    let state = make_state();
    install_inbox_store(&state).await;
    let receiver = AgentId::random();
    let (mut entry, _rx) = make_entry_with_rx(None, "recv".into());
    entry.identity.config.role = "root".into();
    state
        .registry
        .write()
        .await
        .register(receiver.clone(), RegistryEntry::Live(entry));

    // Off-duty: message buffered to inbox.
    state
        .duty
        .set(receiver.clone(), crate::duty::DutyStatus::OffDuty);
    let _ = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(receiver.clone()),
        Json(MessageRequest {
            text: "first".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(state.inboxes.get().unwrap().len_for(&receiver).await, 1);

    // Back on-duty: message also goes to inbox.
    state
        .duty
        .set(receiver.clone(), crate::duty::DutyStatus::OnDuty);
    let _ = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(receiver.clone()),
        Json(MessageRequest {
            text: "second".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(state.inboxes.get().unwrap().len_for(&receiver).await, 2);

    // Pull both messages.
    let msg = state
        .inboxes
        .get()
        .unwrap()
        .pull_undelivered(&receiver)
        .await
        .unwrap();
    assert!(msg.contains("first"));
    assert!(msg.contains("second"));
}

// -- inbound recording vs refusal: a refused message must not append a
// transcript row (the frontend re-sends refusals), an accepted one records
// exactly one, and an accepted-but-buffered (off-duty) one still records --

use crate::external::ExternalProjector;
use crate::relay::MessageLimits;
use kallip_agora_common::ids::{ConversationId, TagmaId};
use std::sync::Arc;
use tempfile::TempDir;

/// Install an external projector with a real (tempdir) history store and
/// subscribe to its frames. Returns the db handle, the frame receiver, and
/// the tempdir (keep it alive for the test's duration).
async fn install_projector(
    state: &crate::state::SharedState,
) -> (
    crate::relay::chat_history::Db,
    tokio::sync::broadcast::Receiver<crate::external::ExternalFrame>,
    TempDir,
) {
    let dir = TempDir::new().unwrap();
    let db = crate::relay::chat_history::open(&dir.path().join("h.sqlite"))
        .await
        .unwrap();
    let projector = ExternalProjector::new(
        Arc::downgrade(state),
        Some(db.clone()),
        Some(ConversationId::for_tagma(&TagmaId::from("t".to_string()))),
        Some(TagmaId::from("t".to_string())),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    let rx = projector.subscribe();
    let _ = state.external.set(projector);
    (db, rx, dir)
}

async fn operator_rows(
    db: &crate::relay::chat_history::Db,
) -> Vec<crate::relay::chat_history::HistoryRow> {
    crate::relay::chat_history::read_last_n(db, None, 10)
        .await
        .unwrap()
}

/// A parked refusal appends nothing: no transcript row, no UserMessage
/// frame — so a client that wakes and re-sends cannot pile up rows.
#[tokio::test]
async fn parked_refusal_records_no_history_row() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, mut rx, _dir) = install_projector(&state).await;
    let root = AgentId::random();
    let (entry, _rx) = make_entry_with_rx(None, "root".into());
    {
        let mut reg = state.registry.write().await;
        reg.register_root(root.clone(), RegistryEntry::Live(entry))
            .unwrap();
        let live = reg.get(&root).unwrap().as_live().unwrap();
        live.agent.state.store(
            crate::state::AgentState::PARKED,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    let err = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(root),
        Json(MessageRequest { text: "hi".into() }),
    )
    .await
    .expect_err("parked root rejects ordinary messages");
    assert_eq!(err.status, 409);

    assert!(
        operator_rows(&db).await.is_empty(),
        "a refused message must not append a transcript row"
    );
    assert!(
        rx.try_recv().is_err(),
        "a refused message must not publish a UserMessage frame"
    );
}

/// Refused attempts leave no rows; the accepted re-send records exactly one
/// — the exactly-once property the frontend's wake-and-retry relies on.
#[tokio::test]
async fn resend_after_refusal_records_single_row() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, mut rx, _dir) = install_projector(&state).await;
    let root = AgentId::random();
    let (entry, _rx) = make_entry_with_rx(None, "root".into());
    {
        let mut reg = state.registry.write().await;
        reg.register_root(root.clone(), RegistryEntry::Live(entry))
            .unwrap();
        let live = reg.get(&root).unwrap().as_live().unwrap();
        live.agent.state.store(
            crate::state::AgentState::PARKED,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    for _ in 0..2 {
        let err = send_message(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(root.clone()),
            Json(MessageRequest { text: "hi".into() }),
        )
        .await
        .expect_err("parked root rejects ordinary messages");
        assert_eq!(err.status, 409);
        assert!(operator_rows(&db).await.is_empty());
    }

    // Un-park (what the wake kick turn does): the next send is accepted.
    {
        let reg = state.registry.read().await;
        let live = reg.get(&root).unwrap().as_live().unwrap();
        live.agent.state.store(
            crate::state::AgentState::IDLE,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    let resp = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(root),
        Json(MessageRequest { text: "hi".into() }),
    )
    .await
    .expect("idle root accepts the message");
    assert_eq!(resp.0, StatusCode::ACCEPTED);

    let rows = operator_rows(&db).await;
    assert_eq!(rows.len(), 1, "exactly one row across refusals + accept");
    assert!(
        rx.try_recv().is_ok(),
        "the accepted send publishes its UserMessage frame"
    );
    assert!(
        rx.try_recv().is_err(),
        "no second frame: refusals published nothing"
    );
}

/// An off-duty agent accepts-and-buffers; the inbound row must still be
/// recorded (recording follows acceptance, not delivery to the agent).
#[tokio::test]
async fn off_duty_message_still_recorded() {
    let state = make_state();
    install_inbox_store(&state).await;
    let (db, _rx, _dir) = install_projector(&state).await;
    let root = AgentId::random();
    let (entry, _rx) = make_entry_with_rx(None, "root".into());
    {
        let mut reg = state.registry.write().await;
        reg.register_root(root.clone(), RegistryEntry::Live(entry))
            .unwrap();
    }
    state
        .duty
        .set(root.clone(), crate::duty::DutyStatus::OffDuty);

    let resp = send_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(root),
        Json(MessageRequest {
            text: "later".into(),
        }),
    )
    .await
    .expect("off-duty send is accepted (buffered)");
    assert_eq!(resp.0, StatusCode::ACCEPTED);

    let rows = operator_rows(&db).await;
    assert_eq!(rows.len(), 1, "buffered-but-accepted message still records");
}
