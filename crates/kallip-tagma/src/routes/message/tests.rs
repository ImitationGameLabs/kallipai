use super::*;
use crate::auth::{AuthIdentity, Identity};
use crate::state::AgentId;
use crate::test_helpers::{add_faulted_root, install_inbox_store, make_entry_with_rx, make_state};
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
