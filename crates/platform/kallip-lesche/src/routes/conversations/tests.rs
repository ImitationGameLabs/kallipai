use super::*;
use crate::test_support::{make_state, seed_presence};
use kallip_agora_common::bytes::{Ciphertext, Ed25519PublicKey, X25519PublicKey};
use kallip_agora_common::ids::{
    ChannelId, ConversationId, ParticipantId, ParticipantKind, TagmaId, TraceId, UserId,
};
use kallip_agora_common::principal::Principal;
use kallip_lesche_common::control::KeyExchangeInit;
use kallip_lesche_common::message::Participant;
use kallip_lesche_common::tunnel::TunnelInbound;
use time::OffsetDateTime;

fn user(name: &str) -> UserId {
    UserId::from(name.to_string())
}

fn dummy_x25519() -> X25519PublicKey {
    X25519PublicKey(vec![0u8; 32])
}

fn dummy_response() -> kallip_lesche_common::control::KeyExchangeResponse {
    kallip_lesche_common::control::KeyExchangeResponse {
        ephemeral_public: X25519PublicKey(vec![1u8; 32]),
        signature: kallip_agora_common::bytes::Ed25519Signature(vec![2u8; 64]),
    }
}

/// Seed an enrolled tagma + presence, return the pieces KEX tests need.
fn seed_fixture(
    state: &SharedConvState,
    control: &crate::test_support::MockControlPlane,
    owner: &UserId,
) -> (
    TagmaId,
    ConversationId,
    tokio::sync::broadcast::Sender<TunnelInbound>,
) {
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(
        &tagma,
        owner.clone(),
        Ed25519PublicKey(vec![0u8; 32]),
        "tok",
    );
    // Provision the conversation record (as a live create_conversation would).
    let conv = {
        let mut reg = state.write().unwrap();
        reg.ensure_conversation(owner, &tagma)
    };
    let (tx, _id) = seed_presence(state, &tagma, owner.clone());
    (tagma, conv, tx)
}

#[tokio::test]
async fn create_conversation_resolves_and_is_idempotent() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(
        &tagma,
        owner.clone(),
        Ed25519PublicKey(vec![0u8; 32]),
        "tok",
    );
    let expected = ConversationId::for_tagma(&tagma).to_string();

    let Json(resp) = create_conversation(
        State(state.clone()),
        AuthPrincipal(Principal::User(owner.clone())),
        Json(CreateConversationRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("resolve");
    assert_eq!(resp.conversation_id, expected);

    // Idempotent: a second resolve returns the same id and leaves the record.
    let Json(resp2) = create_conversation(
        State(state.clone()),
        AuthPrincipal(Principal::User(owner)),
        Json(CreateConversationRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("repeat resolve");
    assert_eq!(resp2.conversation_id, expected);
}

#[tokio::test]
async fn create_conversation_non_owner_404() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let other = user("other");
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");
    let err = create_conversation(
        State(state),
        AuthPrincipal(Principal::User(other)),
        Json(CreateConversationRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect_err("non-owner 404");
    assert_eq!(err.status, 404);
}

/// Parity with the old prod `tagma_resolvable_by`, which did NOT check
/// `revoked` (only owner + enrolled): the owner may still open a bilateral
/// conversation with their own enrolled-but-revoked tagma. The old MOCK was
/// stricter (it checked revoked); this test locks the prod-faithful behavior
/// now that the predicate lives in the relay.
#[tokio::test]
async fn create_conversation_succeeds_for_enrolled_owned_revoked_tagma() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(
        &tagma,
        owner.clone(),
        Ed25519PublicKey(vec![0u8; 32]),
        "tok",
    );
    control.revoke_tagma(&tagma);
    let Json(resp) = create_conversation(
        State(state),
        AuthPrincipal(Principal::User(owner)),
        Json(CreateConversationRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("owner may open a bilateral chat with their own enrolled tagma");
    assert!(!resp.conversation_id.is_empty());
}

/// Existence-oracle preservation at the bilateral-create gate: an unknown
/// tagma and an enrolled-but-not-owned tagma produce the byte-identical 404
/// body (no leak of *why*).
#[tokio::test]
async fn create_conversation_oracle_body_is_uniform_across_failure_modes() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let other = user("other");
    let owned = TagmaId::from("owned-1".to_string());
    control.enroll_tagma(&owned, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");

    // Unknown tagma (not seeded) vs. enrolled-but-not-owned (other is not the
    // owner) -- both collapse to the same "unknown tagma" 404.
    let e_unknown = create_conversation(
        State(state.clone()),
        AuthPrincipal(Principal::User(other.clone())),
        Json(CreateConversationRequest {
            tagma_id: "ghost".to_string(),
        }),
    )
    .await
    .expect_err("unknown tagma");
    let e_non_owner = create_conversation(
        State(state),
        AuthPrincipal(Principal::User(other)),
        Json(CreateConversationRequest {
            tagma_id: owned.to_string(),
        }),
    )
    .await
    .expect_err("non-owner 404");
    assert_eq!(e_unknown.status, 404);
    assert_eq!(e_unknown.message, e_non_owner.message);
}

#[tokio::test]
async fn post_envelope_conversation_id_mismatch_400() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let env = Envelope {
        channel_id: ChannelId::from("other".to_string()),
        sender: Participant {
            id: ParticipantId::for_user(&owner),
            kind: ParticipantKind::Human,
            handle: "Owner".into(),
            tagma_id: None,
        },
        sequence_n: 1,
        trace_id: TraceId::from("t".to_string()),
        timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        ciphertext: Ciphertext(vec![0u8; 16]),
    };
    let err = post_envelope(
        State(state),
        AuthPrincipal(Principal::User(owner)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect_err("mismatch 400");
    assert_eq!(err.status, 400);
}

/// Build a minimal envelope with the given sender attributed to `conv`.
/// Ciphertext/seq are irrelevant to routing (the relay never decrypts).
fn envelope(conv: &ConversationId, sender: Participant) -> Envelope {
    Envelope {
        channel_id: ChannelId::from(conv.as_ref().to_string()),
        sender,
        sequence_n: 1,
        trace_id: TraceId::from("t".to_string()),
        timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        ciphertext: Ciphertext(vec![0u8; 16]),
    }
}

fn human(id: ParticipantId) -> Participant {
    Participant {
        id,
        kind: ParticipantKind::Human,
        handle: "h".into(),
        tagma_id: None,
    }
}

fn agent(id: ParticipantId) -> Participant {
    Participant {
        id,
        kind: ParticipantKind::Agent,
        handle: "a".into(),
        tagma_id: None,
    }
}

/// app -> tagma: the owner posts their own Human envelope, and it lands on
/// the tagma's tunnel.
#[tokio::test]
async fn post_envelope_user_to_tagma_accepted() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (_tagma, conv, tx) = seed_fixture(&state, &control, &owner);
    let mut rx = tx.subscribe();
    let env = envelope(&conv, human(ParticipantId::for_user(&owner)));
    let status = post_envelope(
        State(state),
        AuthPrincipal(Principal::User(owner)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect("user->tagma accepted");
    assert_eq!(status, StatusCode::ACCEPTED);
    match rx.recv().await.expect("tunnel delivery") {
        TunnelInbound::Envelope { .. } => {}
        other => panic!("expected Envelope on tunnel, got {other:?}"),
    }
}

/// A non-owner user posting to the owner's conversation is a non-participant:
/// 404 (existence-oracle), not 403.
#[tokio::test]
async fn post_envelope_user_non_owner_404() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let other = user("other");
    let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let env = envelope(&conv, human(ParticipantId::for_user(&other)));
    let err = post_envelope(
        State(state),
        AuthPrincipal(Principal::User(other)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect_err("non-owner 404");
    assert_eq!(err.status, 404);
}

/// A user may not post an Agent-sender envelope (the kind conjunct).
#[tokio::test]
async fn post_envelope_user_agent_sender_403() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let env = envelope(
        &conv,
        agent(ParticipantId::for_tagma(&TagmaId::from("x".to_string()))),
    );
    let err = post_envelope(
        State(state),
        AuthPrincipal(Principal::User(owner)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect_err("agent sender from user 403");
    assert_eq!(err.status, 403);
}

/// THE BUG: the tagma echoes/replays an owner row as a Human-sender
/// envelope. The relay must accept it (was 403) and deliver it to the
/// owner's app stream.
#[tokio::test]
async fn post_envelope_tagma_echoes_owner_row_accepted() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let app_tx = state.write().unwrap().open_app_stream(&owner);
    let mut app_rx = app_tx.subscribe();
    let env = envelope(&conv, human(ParticipantId::for_user(&owner)));
    let status = post_envelope(
        State(state),
        AuthPrincipal(Principal::Tagma(tagma)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect("tagma echo accepted");
    assert_eq!(status, StatusCode::ACCEPTED);
    match app_rx.recv().await.expect("app stream delivery") {
        LescheEvent::Envelope { .. } => {}
        other => panic!("expected Envelope on app stream, got {other:?}"),
    }
}

/// The tagma's own Agent reply routes to the owner's app stream.
#[tokio::test]
async fn post_envelope_tagma_agent_reply_accepted() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let app_tx = state.write().unwrap().open_app_stream(&owner);
    let mut app_rx = app_tx.subscribe();
    let env = envelope(&conv, agent(ParticipantId::for_tagma(&tagma)));
    let status = post_envelope(
        State(state),
        AuthPrincipal(Principal::Tagma(tagma)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect("agent reply accepted");
    assert_eq!(status, StatusCode::ACCEPTED);
    match app_rx.recv().await.expect("app stream delivery") {
        LescheEvent::Envelope { .. } => {}
        other => panic!("expected Envelope on app stream, got {other:?}"),
    }
}

/// The tagma arm does NOT fence on `sender.id`: history replay must tolerate
/// legacy/derived ids the tagma cannot re-derive (it does not know the
/// owner's raw user_id). A tagma-posted envelope with a non-derived sender id
/// is accepted and routed to the owner's app stream (ownership is the gate;
/// the route target is owner-derived).
#[tokio::test]
async fn post_envelope_tagma_legacy_sender_id_accepted() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let app_tx = state.write().unwrap().open_app_stream(&owner);
    let mut app_rx = app_tx.subscribe();
    // A Human-sender id that is NOT the v5 derivation of the owner (e.g. a
    // legacy raw id stored in chat_history). Accepted all the same.
    let env = envelope(
        &conv,
        human(ParticipantId::from("legacy-user-id".to_string())),
    );
    let status = post_envelope(
        State(state),
        AuthPrincipal(Principal::Tagma(tagma)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect("legacy sender id accepted");
    assert_eq!(status, StatusCode::ACCEPTED);
    match app_rx.recv().await.expect("app stream delivery") {
        LescheEvent::Envelope { .. } => {}
        other => panic!("expected Envelope on app stream, got {other:?}"),
    }
}

/// A different tagma (not the conversation's tagma) posting, even with its
/// own legitimate Agent sender, is a non-participant: 404 (existence-oracle).
#[tokio::test]
async fn post_envelope_wrong_tagma_404() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let intruder = TagmaId::from("tagma-intruder".to_string());
    control.enroll_tagma(
        &intruder,
        owner.clone(),
        Ed25519PublicKey(vec![0u8; 32]),
        "tok",
    );
    let env = envelope(&conv, agent(ParticipantId::for_tagma(&intruder)));
    let err = post_envelope(
        State(state),
        AuthPrincipal(Principal::Tagma(intruder)),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect_err("wrong tagma 404");
    assert_eq!(err.status, 404);
}

/// An admin is not a conversation participant.
#[tokio::test]
async fn post_envelope_admin_404() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let env = envelope(&conv, human(ParticipantId::for_user(&owner)));
    let err = post_envelope(
        State(state),
        AuthPrincipal(Principal::Admin),
        Path(conv.to_string()),
        Json(env),
    )
    .await
    .expect_err("admin 404");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn kex_normal_round_trip() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (tagma, conv, tx) = seed_fixture(&state, &control, &owner);
    let mut rx = tx.subscribe();
    let init = KeyExchangeInit {
        ephemeral_public: dummy_x25519(),
    };

    let state_for_init = state.clone();
    let owner_for_init = owner.clone();
    let conv_for_init = conv.clone();
    let handle = tokio::spawn(async move {
        key_exchange_init(
            State(state_for_init),
            AuthPrincipal(Principal::User(owner_for_init)),
            Path(conv_for_init.to_string()),
            Json(init),
        )
        .await
    });

    let inbound = rx.recv().await.expect("tunnel message");
    let forwarded_conv = match inbound {
        TunnelInbound::KeyExchange {
            conversation_id, ..
        } => conversation_id,
        other => panic!("expected KeyExchange, got {other:?}"),
    };
    assert_eq!(forwarded_conv, conv);

    let expected = dummy_response();
    let resp = key_exchange_response(
        State(state.clone()),
        AuthPrincipal(Principal::Tagma(tagma)),
        Path(conv.to_string()),
        Json(expected.clone()),
    )
    .await
    .expect("response accepted");
    assert_eq!(resp, StatusCode::NO_CONTENT);

    let got = handle.await.unwrap().expect("init ok").0;
    assert_eq!(got.ephemeral_public.0, expected.ephemeral_public.0);
}

#[tokio::test]
async fn kex_duplicate_init_returns_409() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (_tagma, conv, tx) = seed_fixture(&state, &control, &owner);
    let mut rx = tx.subscribe();

    let state_for_init = state.clone();
    let owner_for_init = owner.clone();
    let conv_for_init = conv.clone();
    let handle = tokio::spawn(async move {
        key_exchange_init(
            State(state_for_init),
            AuthPrincipal(Principal::User(owner_for_init)),
            Path(conv_for_init.to_string()),
            Json(KeyExchangeInit {
                ephemeral_public: dummy_x25519(),
            }),
        )
        .await
    });
    let _ = rx.recv().await;

    let err = key_exchange_init(
        State(state.clone()),
        AuthPrincipal(Principal::User(owner)),
        Path(conv.to_string()),
        Json(KeyExchangeInit {
            ephemeral_public: X25519PublicKey(vec![3u8; 32]),
        }),
    )
    .await
    .expect_err("dup 409");
    assert_eq!(err.status, 409);
    handle.abort();
}

#[tokio::test]
async fn kex_timeout_returns_504() {
    let (state, control) = make_state(60, std::time::Duration::from_millis(200));
    let owner = user("owner");
    let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let err = key_exchange_init(
        State(state.clone()),
        AuthPrincipal(Principal::User(owner)),
        Path(conv.to_string()),
        Json(KeyExchangeInit {
            ephemeral_public: dummy_x25519(),
        }),
    )
    .await
    .expect_err("timeout 504");
    assert_eq!(err.status, 504);
    assert!(
        !state
            .pending_key_exchange
            .lock()
            .unwrap()
            .contains_key(&conv),
        "timeout must free the slot"
    );
}

#[tokio::test]
async fn kex_response_without_pending_returns_409() {
    let (state, control) = make_state(60, std::time::Duration::from_secs(2));
    let owner = user("owner");
    let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
    let err = key_exchange_response(
        State(state),
        AuthPrincipal(Principal::Tagma(tagma)),
        Path(conv.to_string()),
        Json(dummy_response()),
    )
    .await
    .expect_err("no pending 409");
    assert_eq!(err.status, 409);
}
