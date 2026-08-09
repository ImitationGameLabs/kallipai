//! Operation-level tests for the in-process relay: a mock lesche that
//! captures posted envelopes, driven by a real `AppState` with a minimal
//! root agent. The initiator side is simulated inline (dir-0 encrypt of the
//! request, dir-1 decrypt of the replies). This proves the semantic channel
//! — encrypt -> relay op -> in-process tagma call -> encrypt reply -> decrypt
//! — without the real agora or any TS. Adapted from the former standalone
//! connector's HTTP-mock-tagma tests, now exercising `deliver_message` and
//! the broadcast pump directly.

use super::*;
use axum::extract::State;
use axum::{Router, routing::post};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{
    ConversationId, ParticipantId, ParticipantKind, RoomId, TagmaId, TraceId, UserId,
};
use kallip_common::protocol::{AuthoredEvent, SignalEvent, SseEvent};
use kallip_e2ee::{
    DIR_INITIATOR_TO_RESPONDER, DIR_RESPONDER_TO_INITIATOR, DeviceKey, SessionKey, nonce,
};
use kallip_lesche_common::control::KeyExchangeInit;
use kallip_lesche_common::message::{Envelope, Participant, RoomMessage, TagmaReply, TagmaRequest};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::state::RegistryEntry;
use crate::test_helpers::{make_entry_with_rx, make_state};

/// Captured outbound envelopes, in arrival order.
type Capture = Arc<Mutex<Vec<Envelope>>>;

/// Captured system signals (busy/idle/terminals/errors), in arrival order.
type SignalCapture = Arc<Mutex<Vec<SignalEvent>>>;

/// Initiator-side encrypt (direction 0 = initiator->responder). The e2e
/// crate's `encrypt` is hardcoded to the responder's direction (1), so the
/// test initiator side builds the AEAD with an explicit direction via the
/// shared `nonce` + `DIR_*`.
fn initiator_encrypt(key: &[u8; 32], seq: u64, pt: &[u8]) -> Vec<u8> {
    let aead = ChaCha20Poly1305::new(key.into());
    aead.encrypt(&Nonce::from(nonce(DIR_INITIATOR_TO_RESPONDER, seq)), pt)
        .unwrap()
}

/// Initiator-side decrypt (direction 1 = responder->initiator).
fn initiator_decrypt(key: &[u8; 32], seq: u64, ct: &[u8]) -> Option<Vec<u8>> {
    let aead = ChaCha20Poly1305::new(key.into());
    aead.decrypt(&Nonce::from(nonce(DIR_RESPONDER_TO_INITIATOR, seq)), ct)
        .ok()
}

async fn spawn_lesche(capture: Capture, signals: SignalCapture) -> String {
    let app = Router::new()
        .route("/v1/conversations/{conv}/envelopes", post(capture_handler))
        .route("/v1/tagmata/{_tagma}/signal", post(signal_handler))
        .route("/v1/tagmata/{_tagma}/status", post(|| async { "ok" }))
        .with_state((capture, signals));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn capture_handler(
    State((c, _)): State<(Capture, SignalCapture)>,
    env: axum::Json<Envelope>,
) -> &'static str {
    c.lock().await.push(env.0);
    "ok"
}

async fn signal_handler(
    State((_, s)): State<(Capture, SignalCapture)>,
    axum::Json(event): axum::Json<SignalEvent>,
) -> &'static str {
    s.lock().await.push(event);
    "ok"
}

/// Build a relay wired to a fresh mock lesche and a real `AppState` whose
/// single root agent has a capturable prompt channel and a `events_cap`-deep
/// event buffer. A pre-shared session key is installed (KEX itself is
/// covered by e2e tests). Returns the handle, the key, the envelope capture,
/// the root's prompt receiver, the root id, and the state.
async fn setup(
    events_cap: usize,
) -> (
    RelayHandle,
    SessionKey,
    Capture,
    mpsc::Receiver<String>,
    AgentId,
    SharedState,
) {
    let (handle, key, capture, _signals, prompt_rx, root_id, state, _db) =
        setup_inner(events_cap, None).await;
    (handle, key, capture, prompt_rx, root_id, state)
}

/// Like [`setup`] but also exposes the captured system signals (for tests
/// that assert on the plaintext signal channel).
async fn setup_with_signals(
    events_cap: usize,
) -> (
    RelayHandle,
    SessionKey,
    Capture,
    SignalCapture,
    mpsc::Receiver<String>,
    AgentId,
    SharedState,
) {
    let (handle, key, capture, signals, prompt_rx, root_id, state, _db) =
        setup_inner(events_cap, None).await;
    (handle, key, capture, signals, prompt_rx, root_id, state)
}

/// Like [`setup`] but with a real tempfile-backed chat-history store shared
/// with the projector, for `TagmaControl::History` replay tests. Returns the
/// `Db` (so tests can append rows the projector will read) and the `TempDir`
/// (to keep the file alive for the test's duration).
async fn setup_with_history(
    events_cap: usize,
) -> (
    RelayHandle,
    SessionKey,
    Capture,
    mpsc::Receiver<String>,
    AgentId,
    SharedState,
    chat_history::Db,
    TempDir,
) {
    let dir = TempDir::new().unwrap();
    let db = chat_history::open(&dir.path().join("history.sqlite"))
        .await
        .unwrap();
    let (handle, key, capture, _signals, prompt_rx, root_id, state, db) =
        setup_inner(events_cap, Some(db.clone())).await;
    (
        handle,
        key,
        capture,
        prompt_rx,
        root_id,
        state,
        db.unwrap(),
        dir,
    )
}

async fn setup_inner(
    events_cap: usize,
    history: Option<chat_history::Db>,
) -> (
    RelayHandle,
    SessionKey,
    Capture,
    SignalCapture,
    mpsc::Receiver<String>,
    AgentId,
    SharedState,
    Option<chat_history::Db>,
) {
    let state = make_state();
    let root_id = AgentId::from("root".to_string());
    let (mut entry, prompt_rx) = make_entry_with_rx(None, "root-tok".to_string());
    // Give the pump enough buffer that a burst of sends does not overflow
    // before the spawned pump task drains.
    let (events_tx, _) = broadcast::channel(events_cap);
    entry.agent.events_tx = events_tx;
    {
        let mut registry = state.registry.write().await;
        registry
            .register_root(root_id.clone(), RegistryEntry::Live(entry))
            .expect("register root");
    }

    let capture: Capture = Arc::new(Mutex::new(Vec::new()));
    let signals: SignalCapture = Arc::new(Mutex::new(Vec::new()));
    let lesche_url = spawn_lesche(capture.clone(), signals.clone()).await;
    let client = LescheClient::builder(&lesche_url, "tok").build().unwrap();
    let device = DeviceKey::generate();
    let tagma_id = TagmaId::from("tagma".to_string());
    let conversation_id = ConversationId::for_tagma(&tagma_id);
    // Install the external projector with the (optional) history store +
    // conversation id, so the relay's history reads (handle_history) and
    // inbound persistence (deliver_message → record_inbound) go through the
    // same sole-writer the production boot installs.
    let projector = crate::external::ExternalProjector::new(
        Arc::downgrade(&state),
        history.clone(),
        Some(conversation_id.clone()),
        Some(tagma_id.clone()),
        Some("Tagma".into()),
        MessageLimits::default(),
    );
    if state.external.set(projector).is_err() {
        panic!("projector must be installed once per test state");
    }
    let handle = RelayHandle::new(
        client,
        tagma_id,
        "Tagma".into(),
        device,
        root_id.clone(),
        Arc::downgrade(&state),
    );
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("getrandom");
    let key = SessionKey::new(key);
    handle.inner.crypto.lock().await.key = Some(key.clone());
    (
        handle, key, capture, signals, prompt_rx, root_id, state, history,
    )
}

/// Encrypt an app->tagma payload (a serialized `TagmaRequest`) into a
/// user-sender envelope, dir 0 (initiator->responder).
fn payload_envelope(key: &SessionKey, conv: &ConversationId, seq: u64, bytes: &[u8]) -> Envelope {
    Envelope {
        conversation_id: conv.clone(),
        sender: Participant {
            id: ParticipantId::for_user(&UserId::from("u".to_string())),
            kind: ParticipantKind::Human,
            handle: "Alice".into(),
            tagma_id: None,
        },
        sequence_n: seq,
        trace_id: TraceId::from("t".to_string()),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(initiator_encrypt(key, seq, bytes)),
    }
}

fn user_envelope(
    key: &SessionKey,
    conv: &ConversationId,
    seq: u64,
    request: TagmaRequest,
) -> Envelope {
    let bytes = serde_json::to_vec(&request).unwrap();
    payload_envelope(key, conv, seq, &bytes)
}

/// Decrypt the captured envelopes into replies. Delegates to
/// [`drain_with_senders`] and drops the wire sender.
async fn drain_replies(capture: &Capture, key: &SessionKey) -> Vec<TagmaReply> {
    drain_with_senders(capture, key)
        .await
        .into_iter()
        .map(|(_, reply)| reply)
        .collect()
}

/// Decrypt the captured envelopes into `(sender, reply)` pairs, retaining each
/// envelope's wire `sender` so a test can assert per-row sender attribution
/// (the refactor threads the stored sender onto each emitted history frame's
/// outer envelope).
async fn drain_with_senders(capture: &Capture, key: &SessionKey) -> Vec<(Participant, TagmaReply)> {
    capture
        .lock()
        .await
        .clone()
        .into_iter()
        .map(|env| {
            let plain = initiator_decrypt(key, env.sequence_n, &env.ciphertext.0).unwrap();
            let reply = serde_json::from_slice::<TagmaReply>(&plain).unwrap();
            (env.sender, reply)
        })
        .collect()
}

/// Resolve the relay's conversation id (derived from the tagma id).
fn conv_of(handle: &RelayHandle) -> ConversationId {
    handle.inner.conversation_id.clone()
}

/// The test peer (matches `payload_envelope`'s sender): the relay user whose
/// partition the history reads filter to.
fn peer() -> Participant {
    Participant {
        id: ParticipantId::for_user(&UserId::from("u".to_string())),
        kind: ParticipantKind::Human,
        handle: "Alice".into(),
        tagma_id: None,
    }
}

#[tokio::test]
async fn send_message_round_trips() {
    let (handle, key, capture, mut prompt_rx, _root_id, _state) = setup(1).await;
    let conv = conv_of(&handle);
    handle
        .handle_user_op(user_envelope(
            &key,
            &conv,
            1,
            TagmaRequest::SendMessage {
                req_id: 10,
                text: "hello".into(),
            },
        ))
        .await;
    // The root agent's prompt channel received the text, prefixed with the
    // `[From: user <handle>]` header (a Human sender carries its handle).
    let delivered = prompt_rx.recv().await.expect("message delivered");
    assert_eq!(delivered, "[From: user Alice]\nhello");
    // The app got a MessageAccepted reply.
    let replies = drain_replies(&capture, &key).await;
    assert!(matches!(
        replies.as_slice(),
        [TagmaReply::MessageAccepted {
            req_id: 10,
            queue_depth: 0,
            ..
        }]
    ));
}

#[tokio::test]
async fn interrupt_round_trips() {
    let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
    let conv = conv_of(&handle);
    handle
        .handle_user_op(user_envelope(
            &key,
            &conv,
            1,
            TagmaRequest::Interrupt { req_id: 7 },
        ))
        .await;
    // interrupt_root is a no-op against the minimal root (no active round
    // token); the relay still emits the Interrupted ack.
    let replies = drain_replies(&capture, &key).await;
    assert!(matches!(
        replies.as_slice(),
        [TagmaReply::Interrupted { req_id: 7 }]
    ));
}

#[tokio::test]
async fn op_before_key_exchange_is_dropped() {
    // A relay with no session key must drop the op silently (no in-process
    // call, no reply).
    let (handle, _key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
    // Wipe the installed key so the epoch is empty.
    handle.inner.crypto.lock().await.key = None;
    let conv = conv_of(&handle);
    handle
        .handle_user_op(user_envelope(
            &key_zero(),
            &conv,
            1,
            TagmaRequest::SendMessage {
                req_id: 1,
                text: "x".into(),
            },
        ))
        .await;
    assert!(capture.lock().await.is_empty(), "no reply before KEX");
}

/// The first message of a crypto epoch carries `sequence_n = 0` and MUST be
/// accepted (a plain `u64` window initialized to 0 would reject it as
/// `0 <= 0`). The window is `None` until the first message lands, so seq=0
/// passes; the same `None` state is restored on every KEX reset.
#[tokio::test]
async fn first_inbound_seq_zero_of_an_epoch_is_accepted() {
    let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
    let conv = conv_of(&handle);
    handle
        .handle_user_op(user_envelope(
            &key,
            &conv,
            0,
            TagmaRequest::SendMessage {
                req_id: 1,
                text: "first of epoch".into(),
            },
        ))
        .await;
    let replies = drain_replies(&capture, &key).await;
    assert_eq!(
        replies.len(),
        1,
        "the first seq=0 of an epoch must be accepted and produce a reply"
    );
}

#[tokio::test]
async fn replayed_inbound_envelope_is_dropped() {
    let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
    let conv = conv_of(&handle);
    let env = user_envelope(
        &key,
        &conv,
        5,
        TagmaRequest::SendMessage {
            req_id: 1,
            text: "first".into(),
        },
    );
    handle.handle_user_op(env.clone()).await;
    // A replay of the same sequence number is dropped without a second reply.
    handle.handle_user_op(env).await;
    let replies = drain_replies(&capture, &key).await;
    assert_eq!(
        replies.len(),
        1,
        "replayed seq must not produce a second reply"
    );
}

#[tokio::test]
async fn garbage_ciphertext_does_not_advance_replay_window() {
    // A forged envelope with a huge `sequence_n` and undecryptable
    // ciphertext must NOT advance the replay high-water-mark. If it did,
    // every later legitimate envelope (seq < u64::MAX) would be silently
    // dropped as a replay for the rest of the epoch — a one-shot blind.
    let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
    let conv = conv_of(&handle);
    let forged = Envelope {
        conversation_id: conv.clone(),
        sender: Participant {
            id: ParticipantId::for_user(&UserId::from("u".to_string())),
            kind: ParticipantKind::Human,
            handle: "Alice".into(),
            tagma_id: None,
        },
        sequence_n: u64::MAX,
        trace_id: TraceId::from("t".to_string()),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(vec![0u8; 16]),
    };
    handle.handle_user_op(forged).await;
    // A normal legitimate envelope must still produce a reply.
    handle
        .handle_user_op(user_envelope(
            &key,
            &conv,
            1,
            TagmaRequest::SendMessage {
                req_id: 1,
                text: "after forge".into(),
            },
        ))
        .await;
    let replies = drain_replies(&capture, &key).await;
    assert_eq!(
        replies.len(),
        1,
        "undecryptable envelope must not poison the replay window"
    );
}

#[tokio::test]
async fn pump_splits_authored_envelope_and_system_signal() {
    let events = vec![
        SseEvent::Busy,
        SseEvent::AssistantContent {
            content: "hi".into(),
        },
        SseEvent::Idle,
        SseEvent::ToolCall {
            name: "x".into(),
            args: "{}".into(),
        }, // dropped (out of capability)
    ];
    let (handle, key, capture, signals, _prompt_rx, _root_id, state) = setup_with_signals(16).await;
    handle.start_pump().await;

    // Resolve the root's event sender and wait for the pump to subscribe
    // (broadcast::send with no receiver returns Err).
    let events_tx = {
        let registry = state.registry.read().await;
        let (_, entry) = registry.root_agent().expect("root present");
        entry.as_live().expect("root live").agent.events_tx.clone()
    };
    for _ in 0..400 {
        if events_tx.receiver_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for ev in events {
        events_tx.send(ev).expect("pump subscribed");
        // Yield between sends so the pump's recv->emit loop keeps up; the
        // 16-deep buffer makes this a safety margin, not a hard gate.
        tokio::task::yield_now().await;
    }

    // Authored content rides the encrypted envelope channel; busy/idle do
    // NOT (they cross as plaintext signals), so the envelope capture holds
    // exactly one authored reply.
    let mut got = Vec::new();
    for _ in 0..300 {
        got = drain_replies(&capture, &key).await;
        if !got.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // System signals ride the plaintext channel; drain them too.
    let mut sig = Vec::new();
    for _ in 0..300 {
        sig = signals.lock().await.clone();
        if sig.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.stop_pump().await;
    assert_eq!(
        got.len(),
        1,
        "only authored content (AssistantContent) rides the envelope channel"
    );
    assert!(matches!(
        got[0],
        TagmaReply::Event {
            event: AuthoredEvent::AssistantContent { .. },
            ..
        }
    ));
    assert_eq!(sig.len(), 2, "busy/idle cross as plaintext system signals");
    assert!(matches!(sig[0], SignalEvent::Busy));
    assert!(matches!(sig[1], SignalEvent::Idle));
}

#[tokio::test]
async fn re_kex_installs_key_resets_seq_and_starts_pump() {
    // Advance the outbound counter, then a KEX must reset it to 0 and leave
    // a session key installed + a pump running.
    let (handle, _key, _capture, _prompt_rx, _root_id, _state) = setup(1).await;
    {
        let mut c = handle.inner.crypto.lock().await;
        c.outbound_seq = 42;
        c.seen_inbound = Some(42);
    }

    // App side: a real ephemeral keypair so respond_key_exchange succeeds.
    let app_secret = x25519_dalek::ReusableSecret::random();
    let app_pub = x25519_dalek::PublicKey::from(&app_secret);
    let init = KeyExchangeInit {
        ephemeral_public: kallip_agora_common::bytes::X25519PublicKey(app_pub.to_bytes().to_vec()),
    };
    handle.handle_kex(conv_of(&handle), init).await;

    let c = handle.inner.crypto.lock().await;
    assert!(c.key.is_some(), "KEX must install a session key");
    assert_eq!(c.outbound_seq, 0, "KEX must reset the outbound counter");
    assert_eq!(c.seen_inbound, None, "KEX must reset the inbound window");
    drop(c);
    assert!(
        handle.inner.pump.lock().await.is_some(),
        "KEX must start the pump"
    );
    handle.stop_pump().await;
}

/// A zero key for the no-KEX drop test (the ciphertext never decrypts; the
/// op is dropped at the key-absent check before decryption anyway).
fn key_zero() -> SessionKey {
    SessionKey::new([0u8; 32])
}

/// `handle_user_op` with `SendMessage` drives the real `deliver_message`,
/// which calls the projector's `record_inbound`: the inbound row is
/// persisted once under the conversation id. The `MessageAccepted` ack
/// therefore carries `history_id = 0` (the app dedups its optimistic line
/// off the `UserMessage` frame the projector publishes and the pump
/// forwards — exercised in `pump_splits_authored_envelope_and_system_signal`).
/// The row then replays as a `UserMessage` echo under its row id.
#[tokio::test]
async fn send_message_persists_inbound_and_forwards_usermessage() {
    let (handle, key, capture, mut prompt_rx, _root_id, _state, db, _dir) =
        setup_with_history(8).await;
    let conv = conv_of(&handle);
    handle
        .handle_user_op(user_envelope(
            &key,
            &conv,
            1,
            TagmaRequest::SendMessage {
                req_id: 10,
                text: "hi".into(),
            },
        ))
        .await;
    // The root agent received the prompt.
    let _ = prompt_rx.recv().await.expect("message delivered");
    let replies = drain_replies(&capture, &key).await;

    // The ack is present and carries history_id = 0: the inbound row id no
    // longer rides the ack (the projector-published UserMessage frame is the
    // promotion path).
    let ack = replies.iter().find_map(|r| match r {
        TagmaReply::MessageAccepted {
            req_id: 10,
            history_id,
            ..
        } => Some(*history_id),
        _ => None,
    });
    assert_eq!(
        ack,
        Some(0),
        "MessageAccepted ack no longer stamps history_id"
    );

    // The inbound row is persisted under the peer's partition.
    let user = peer();
    let rows = chat_history::read_last_n(&db, Some(user.id.as_ref()), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].direction, "inbound");
    let um_id = rows[0].id;
    assert!(um_id > 0, "inbound row assigned a positive id");

    // It replays as a UserMessage echo under its row id (filtered to the peer).
    capture.lock().await.clear();
    let trace = kallip_agora_common::ids::TraceId::from("h".to_string());
    handle
        .handle_history(&trace, 1, &user, None, None, 50)
        .await;
    let replies = drain_replies(&capture, &key).await;
    let um = replies.iter().find_map(|r| match r {
        TagmaReply::UserMessage {
            history_id, text, ..
        } if *history_id == um_id => Some(text),
        _ => None,
    });
    assert_eq!(
        um,
        Some(&"hi".to_string()),
        "inbound row echoed as UserMessage"
    );
}

/// `handle_history` (latest mode) replays both outbound and inbound rows in
/// id order: outbound as its stored `Event` reply, inbound as a `UserMessage`
/// echo, each stamped with its row id, then a `HistoryBatchEnd` marker.
#[tokio::test]
async fn handle_history_latest_replays_both_directions_in_order() {
    let (handle, key, capture, _prompt_rx, _root_id, _state, db, _dir) =
        setup_with_history(8).await;
    let trace = kallip_agora_common::ids::TraceId::from("test".to_string());
    let user = peer();
    // Outbound, inbound, outbound — interleaved, ids assigned in append order.
    // All seeded under the peer partition (the relay replay filters to it); the
    // agent's identity is not stored (reconstructed at read).
    chat_history::append(
        &db,
        Some(user.id.as_ref()),
        Some(user.handle.as_str()),
        "outbound",
        "o0",
    )
    .await
    .unwrap();
    chat_history::append(
        &db,
        Some(user.id.as_ref()),
        Some(user.handle.as_str()),
        "inbound",
        "u0",
    )
    .await
    .unwrap();
    chat_history::append(
        &db,
        Some(user.id.as_ref()),
        Some(user.handle.as_str()),
        "outbound",
        "o1",
    )
    .await
    .unwrap();

    capture.lock().await.clear();
    handle
        .handle_history(&trace, 7, &user, None, None, 50)
        .await;

    let frames = drain_with_senders(&capture, &key).await;
    // o0, u0, o1, batch-end = 4 frames.
    assert_eq!(frames.len(), 4);
    let mut last = 0i64;
    let mut saw_end = false;
    for (sender, reply) in &frames {
        match reply {
            TagmaReply::Event { history_id, .. } => {
                assert!(
                    *history_id > last,
                    "ids must strictly increase across the batch"
                );
                last = *history_id;
                // An agent-authored row is emitted on an Agent-stamped envelope.
                assert_eq!(
                    sender.kind,
                    ParticipantKind::Agent,
                    "Event row sender must be the agent"
                );
            }
            TagmaReply::UserMessage { history_id, .. } => {
                assert!(
                    *history_id > last,
                    "ids must strictly increase across the batch"
                );
                last = *history_id;
                // A user-authored row carries the user sender on the wire
                // envelope (the per-row attribution this refactor added).
                assert_eq!(
                    sender.kind,
                    ParticipantKind::Human,
                    "UserMessage row sender must be the user"
                );
            }
            TagmaReply::HistoryBatchEnd {
                req_id,
                count,
                more,
            } => {
                assert_eq!(*req_id, 7);
                assert_eq!(*count, 3);
                assert!(!more);
                saw_end = true;
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }
    assert!(saw_end, "batch must end with a HistoryBatchEnd marker");
}

/// `handle_history` (latest mode) reports `more=false` even when the stored
/// row count equals the (capped) request limit: latest is a recent-N
/// snapshot with no further page to pull, so a full page must NOT advertise
/// more. (Guards against the `rows.len() == limit` heuristic leaking into
/// the latest branch.)
#[tokio::test]
async fn handle_history_latest_more_is_false_even_at_full_page() {
    let (handle, key, capture, _prompt_rx, _root_id, _state, db, _dir) =
        setup_with_history(8).await;
    let trace = kallip_agora_common::ids::TraceId::from("test".to_string());
    let user = peer();
    // Insert exactly `limit` rows under the peer partition.
    for i in 0..3 {
        chat_history::append(
            &db,
            Some(user.id.as_ref()),
            Some(user.handle.as_str()),
            "outbound",
            &format!("e{i}"),
        )
        .await
        .unwrap();
    }

    capture.lock().await.clear();
    // Request exactly 3 (== stored count); latest mode.
    handle.handle_history(&trace, 9, &user, None, None, 3).await;

    let replies = drain_replies(&capture, &key).await;
    match replies.last().expect("batch end present") {
        TagmaReply::HistoryBatchEnd { count, more, .. } => {
            assert_eq!(*count, 3);
            assert!(
                !more,
                "latest mode must never advertise more, even at a full page"
            );
        }
        other => panic!("expected batch end, got {other:?}"),
    }
}

/// `handle_history` in `after` mode returns only rows with id > after
/// (incremental catch-up); `before` mode returns the older chunk and sets
/// `more` when truncated by `limit`.
#[tokio::test]
async fn handle_history_after_and_before_windows() {
    let (handle, key, capture, _prompt_rx, _root_id, _state, db, _dir) =
        setup_with_history(8).await;
    let user = peer();
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(
            chat_history::append(
                &db,
                Some(user.id.as_ref()),
                Some(user.handle.as_str()),
                "outbound",
                &format!("e{i}"),
            )
            .await
            .unwrap()
            .0,
        );
    }
    let trace = kallip_agora_common::ids::TraceId::from("test".to_string());

    // after=ids[0] -> ids[1..3] + batch-end; more=false (3 < 50).
    capture.lock().await.clear();
    handle
        .handle_history(&trace, 1, &user, Some(ids[0]), None, 50)
        .await;
    let replies = drain_replies(&capture, &key).await;
    assert_eq!(replies.len(), 4, "after-window: 3 rows + end");
    match &replies[0] {
        TagmaReply::Event { history_id, .. } => assert_eq!(*history_id, ids[1]),
        other => panic!("{other:?}"),
    }

    // before=ids[3] limit 2 -> ids[1], ids[2] + batch-end; more=true (hit limit).
    capture.lock().await.clear();
    handle
        .handle_history(&trace, 2, &user, None, Some(ids[3]), 2)
        .await;
    let replies = drain_replies(&capture, &key).await;
    assert_eq!(replies.len(), 3, "before-window: 2 rows + end");
    match replies.last().unwrap() {
        TagmaReply::HistoryBatchEnd { count, more, .. } => {
            assert_eq!(*count, 2);
            assert!(more, "more must be true when the limit is hit");
        }
        other => panic!("expected batch end, got {other:?}"),
    }
}

/// With no history store configured, `handle_history` emits an empty
/// `HistoryBatchEnd` so the app is not left waiting on its deadline.
#[tokio::test]
async fn handle_history_without_store_emits_empty_batch_end() {
    let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(8).await;
    let trace = kallip_agora_common::ids::TraceId::from("h".to_string());
    let user = peer();
    handle
        .handle_history(&trace, 5, &user, None, None, 50)
        .await;
    let replies = drain_replies(&capture, &key).await;
    assert!(matches!(
        replies.as_slice(),
        [TagmaReply::HistoryBatchEnd {
            req_id: 5,
            count: 0,
            more: false
        }]
    ));
}

/// A room envelope (conversation_id is a joined room) routes to the room path,
/// NOT the bilateral path: the ciphertext is the raw `RoomMessage` JSON (rooms
/// are not bilateral-E2E; the relay stamps the sender), the root agent's prompt
/// receives the room-incoming header, and NO bilateral `MessageAccepted` reply
/// is emitted. This is the refactor's load-bearing routing fork
/// (`handle_user_op`), which had no test driving it.
#[tokio::test]
async fn handle_user_op_routes_joined_room_to_the_room_path() {
    let (handle, _key, capture, _signals, mut prompt_rx, _root_id, state, _history) =
        setup_inner(8, None).await;
    // Seed the joined-rooms cache so the fork recognizes the room.
    let room = RoomId::from("00000000-0000-0000-0000-000000000aa1".to_string());
    state.joined_rooms.set_joined_rooms([room.clone()]).await;

    let envelope = room_envelope(&room, "hi room", "Alice");
    handle.handle_user_op(envelope).await;

    // Fail fast (timeout) if a regression skips the room path instead of
    // hanging the suite on a bare `recv()`.
    let delivered = tokio::time::timeout(Duration::from_millis(100), prompt_rx.recv())
        .await
        .expect("room message delivered to prompt within 100ms")
        .expect("prompt channel not closed");
    // The room header names the sender kind + handle + room id.
    assert!(
        delivered.contains(&format!("| room {room}")),
        "expected room header, got: {delivered}"
    );
    assert!(
        delivered.contains("[From: human Alice"),
        "delivered: {delivered}"
    );
    assert!(delivered.contains("hi room"), "delivered: {delivered}");
    // The room path deliberately skips the bilateral ACK (the lesche's 202 is
    // the room ack), so no envelope is posted.
    assert!(
        capture.lock().await.is_empty(),
        "room path must not bilateral-emit a MessageAccepted"
    );
}

/// A conversation_id that is NOT a joined room skips the room path. It falls
/// through to the bilateral path, whose AEAD decrypt of the raw-JSON ciphertext
/// fails (rooms and bilateral use disjoint wire forms), so no prompt is
/// delivered -- proving the room path was not taken for a non-joined
/// conversation. (This asserts only that the room path is skipped; the
/// bilateral drop itself has several silent short-circuits.)
#[tokio::test]
async fn handle_user_op_skips_room_path_for_non_joined_room() {
    let (handle, _key, _capture, _signals, mut prompt_rx, _root_id, _state, _history) =
        setup_inner(8, None).await;
    let not_joined = RoomId::from("00000000-0000-0000-0000-000000000bb2".to_string());
    let envelope = room_envelope(&not_joined, "hi room", "Alice");
    handle.handle_user_op(envelope).await;
    // Bilateral decrypt of raw bytes fails before any deliver; the room path
    // (which WOULD deliver) was skipped because the room is not joined.
    let leaked = tokio::time::timeout(Duration::from_millis(100), prompt_rx.recv()).await;
    assert!(
        leaked.is_err(),
        "no prompt expected on the bilateral fallthrough, got {leaked:?}"
    );
}

/// Build a room-path envelope: `conversation_id` is the room, the sender is a
/// human peer, and `ciphertext` is the RAW `RoomMessage` JSON (the room path
/// treats `ciphertext.0` as plaintext -- no bilateral decrypt).
fn room_envelope(room: &RoomId, text: &str, handle: &str) -> Envelope {
    let plaintext = serde_json::to_vec(&RoomMessage { text: text.into() }).unwrap();
    Envelope {
        conversation_id: ConversationId::from(room.as_ref().to_string()),
        sender: Participant {
            id: ParticipantId::for_user(&UserId::from("u".to_string())),
            kind: ParticipantKind::Human,
            handle: handle.into(),
            tagma_id: None,
        },
        sequence_n: 1,
        trace_id: TraceId::from("t".to_string()),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(plaintext),
    }
}
