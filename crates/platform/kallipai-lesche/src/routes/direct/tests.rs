//! Direct-session route tests: the create-or-get authz matrix, the send
//! contract (member gate / sender-match / stamp / fan / 202), history
//! windows, and the cursor write -- against ephemeral Postgres + the mock
//! registry, invoking the handlers directly (the rooms tests' harness).

use super::*;
use crate::auth::AuthPrincipal;
use crate::routes::test_support::{as_tagma, db_state};
use kallipai_archeion_common::ids::{ChannelId, TraceId, UserId};
use std::sync::Arc;

fn uid(s: &str) -> UserId {
    UserId::from(s.to_string())
}

fn dummy_key() -> kallipai_archeion_common::bytes::Ed25519PublicKey {
    kallipai_archeion_common::bytes::Ed25519PublicKey([0u8; 32].to_vec())
}

/// Enroll `tagma` under `owner` with a usable pinned key + bearer token.
fn enroll(
    control: &Arc<crate::test_support::MockControlPlane>,
    tagma: &TagmaId,
    owner: &str,
    token: &str,
) {
    control.enroll_tagma(tagma, uid(owner), dummy_key(), token);
}

fn envelope_text(sender: &TagmaId, session: &DirectSessionId, body: &[u8]) -> Envelope {
    Envelope {
        channel_id: ChannelId::from(session.as_ref().to_string()),
        sender: kallipai_lesche_common::message::Participant {
            id: ParticipantId::for_tagma(sender),
            kind: ParticipantKind::Agent,
            handle: "spoofed".to_string(),
            tagma_id: None,
        },
        sequence_n: 0,
        trace_id: TraceId::from("t".to_string()),
        timestamp: time::OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(body.to_vec()),
    }
}

/// Register a live tunnel for `tagma` and return a receiver (kept alive by
/// the caller) so a fan send lands on a live subscriber.
fn tunnel_rx(
    state: &SharedConvState,
    tagma: &TagmaId,
    owner: &UserId,
) -> tokio::sync::broadcast::Receiver<kallipai_lesche_common::tunnel::TunnelInbound> {
    let mut reg = state.registry.write().unwrap();
    let (tx, _keep) = tokio::sync::broadcast::channel(8);
    let rx = tx.subscribe();
    reg.register_presence(tagma, owner.clone(), tx, std::sync::Arc::new(()));
    rx
}

async fn create(
    state: &SharedConvState,
    me: &TagmaId,
    peer: &str,
) -> Result<DirectSessionView, ApiError> {
    match post_direct_session(
        State(state.clone()),
        as_tagma(me),
        Json(CreateDirectSessionRequest {
            peer: peer.to_string(),
        }),
    )
    .await
    {
        Ok(Json(view)) => Ok(view),
        Err(e) => Err(e),
    }
}

#[tokio::test]
async fn create_same_owner_returns_the_derived_session_and_peer_view() {
    let (state, control) = db_state().await;
    let t1 = TagmaId::from("t-1".to_string());
    let t2 = TagmaId::from("t-2".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &t2, "alice", "tok-2");

    let view = create(&state, &t1, t2.as_ref()).await.expect("created");
    assert_eq!(view.session_id, DirectSessionId::for_pair(&t1, &t2));
    assert_eq!(view.peer.tagma_id, t2);
    assert_eq!(
        view.peer.handle,
        agent_handle(&ParticipantId::for_tagma(&t2), "alice")
    );

    // The other side creating is the SAME session (idempotent, both orders).
    let mirror = create(&state, &t2, t1.as_ref())
        .await
        .expect("mirror created");
    assert_eq!(mirror.session_id, view.session_id);
}

#[tokio::test]
async fn create_rejects_unknown_cross_owner_self_and_unusable_uniformly() {
    let (state, control) = db_state().await;
    let t1 = TagmaId::from("t-1".to_string());
    let other_owner = TagmaId::from("t-other".to_string());
    let unpinned = TagmaId::from("t-unpinned".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &other_owner, "bob", "tok-other");
    enroll(&control, &unpinned, "alice", "tok-unpinned");
    control.set_pinned_key(&unpinned, None);
    // A PEER who is enrolled but not tunnel-usable (no pinned key).
    let peer_unusable = TagmaId::from("t-peer-unusable".to_string());
    enroll(&control, &peer_unusable, "alice", "tok-peer-unusable");
    control.set_pinned_key(&peer_unusable, None);

    // Unknown peer and cross-owner peer are the SAME 404 (existence-oracle).
    let err = create(&state, &t1, "tagma-nonexistent").await.unwrap_err();
    assert_eq!(err.status, 404);
    let err = create(&state, &t1, other_owner.as_ref()).await.unwrap_err();
    assert_eq!(err.status, 404);
    // An initiator without a usable pinned key is the same 404.
    let err = create(&state, &unpinned, t1.as_ref()).await.unwrap_err();
    assert_eq!(err.status, 404);
    // A peer-side unusable tagma is the same 404 from the initiator's side.
    let err = create(&state, &t1, peer_unusable.as_ref())
        .await
        .unwrap_err();
    assert_eq!(err.status, 404);
    // Self-session is a 400 (the caller DID name a real id: itself).
    let err = create(&state, &t1, t1.as_ref()).await.unwrap_err();
    assert_eq!(err.status, 400);
    // A non-tagma principal (a user) gets the surface's uniform 404.
    let err = post_direct_session(
        State(state.clone()),
        AuthPrincipal(Principal::User(uid("alice"))),
        Json(CreateDirectSessionRequest {
            peer: t1.as_ref().to_string(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn send_fans_to_the_peer_stamps_the_handle_and_assigns_seq() {
    let (state, control) = db_state().await;
    let alice = uid("alice");
    let t1 = TagmaId::from("t-1".to_string());
    let t2 = TagmaId::from("t-2".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &t2, "alice", "tok-2");
    let view = create(&state, &t1, t2.as_ref()).await.expect("created");

    let mut t2_rx = tunnel_rx(&state, &t2, &alice);

    let status = post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&t1),
        Path(view.session_id.as_ref().to_string()),
        Json(envelope_text(&t1, &view.session_id, b"hello")),
    )
    .await
    .expect("accepted");
    assert_eq!(status, StatusCode::ACCEPTED);

    // The peer's live tunnel received the envelope, with the authoritative
    // stamped handle -- never the client-supplied "spoofed" one.
    let kallipai_lesche_common::tunnel::TunnelInbound::Envelope { envelope } =
        t2_rx.recv().await.expect("peer received the envelope")
    else {
        panic!("expected an envelope");
    };
    assert_eq!(envelope.ciphertext.0, b"hello");
    assert_eq!(
        envelope.sender.handle,
        agent_handle(&ParticipantId::for_tagma(&t1), "alice")
    );
    assert_eq!(envelope.sender.tagma_id, Some(t1.clone()));

    // History has the row back; the windowed read is strictly-after.
    let all = get_direct_session_messages(
        State(state.clone()),
        as_tagma(&t2),
        Path(view.session_id.as_ref().to_string()),
        axum::extract::Query(HistoryQuery::default()),
    )
    .await
    .expect("history");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].seq, 1);
    assert_eq!(all[0].sender.handle, envelope.sender.handle);
    assert_eq!(all[0].ciphertext.0, b"hello");

    post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&t2),
        Path(view.session_id.as_ref().to_string()),
        Json(envelope_text(&t2, &view.session_id, b"hi back")),
    )
    .await
    .expect("accepted");
    let window = get_direct_session_messages(
        State(state.clone()),
        as_tagma(&t1),
        Path(view.session_id.as_ref().to_string()),
        axum::extract::Query(HistoryQuery {
            after_seq: Some(1),
            limit: None,
        }),
    )
    .await
    .expect("windowed history");
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].seq, 2);
    assert_eq!(
        window[0].sender.handle,
        agent_handle(&ParticipantId::for_tagma(&t2), "alice")
    );
}

#[tokio::test]
async fn send_and_read_gates_collapse_to_404_and_sender_mismatch_is_403() {
    let (state, control) = db_state().await;
    let t1 = TagmaId::from("t-1".to_string());
    let t2 = TagmaId::from("t-2".to_string());
    let outsider = TagmaId::from("t-out".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &t2, "alice", "tok-2");
    enroll(&control, &outsider, "alice", "tok-out");
    let view = create(&state, &t1, t2.as_ref()).await.expect("created");
    let path = view.session_id.as_ref().to_string();

    // A non-member (same owner, real tagma) gets the same 404 as an unknown
    // session -- it cannot confirm the session exists.
    let err = post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&outsider),
        Path(path.clone()),
        Json(envelope_text(&outsider, &view.session_id, b"[]")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 404);
    let err = get_direct_session_messages(
        State(state.clone()),
        as_tagma(&outsider),
        Path(path.clone()),
        axum::extract::Query(HistoryQuery::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 404);
    // Unknown session: same 404.
    let err = post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&t1),
        Path("s-none".to_string()),
        Json(envelope_text(
            &t1,
            &DirectSessionId::from("s-none".to_string()),
            b"[]",
        )),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 404);
    // A member may send only as themselves: a spoofed sender id is a 403.
    let err = post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&t1),
        Path(path.clone()),
        Json(envelope_text(&t2, &view.session_id, b"[]")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 403);
    // A body claiming a different session is a 400.
    let other = DirectSessionId::for_pair(&t1, &outsider);
    let err = post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&t1),
        Path(path),
        Json(envelope_text(&t1, &other, b"[]")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 400);
}

#[tokio::test]
async fn cursor_write_is_member_gated_and_returns_no_content() {
    let (state, control) = db_state().await;
    let t1 = TagmaId::from("t-1".to_string());
    let t2 = TagmaId::from("t-2".to_string());
    let outsider = TagmaId::from("t-out".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &t2, "alice", "tok-2");
    enroll(&control, &outsider, "alice", "tok-out");
    let view = create(&state, &t1, t2.as_ref()).await.expect("created");

    let status = put_direct_read_cursor(
        State(state.clone()),
        as_tagma(&t2),
        Path(view.session_id.as_ref().to_string()),
        Json(ReadCursorRequest { last_read_seq: 3 }),
    )
    .await
    .expect("cursor written");
    assert_eq!(status, StatusCode::NO_CONTENT);

    let err = put_direct_read_cursor(
        State(state.clone()),
        as_tagma(&outsider),
        Path(view.session_id.as_ref().to_string()),
        Json(ReadCursorRequest { last_read_seq: 3 }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, 404);

    // The listing reflects the session for both members, with the OTHER side
    // as the peer.
    let mine = list_direct_sessions(State(state.clone()), as_tagma(&t1))
        .await
        .expect("list");
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].session_id, view.session_id);
    assert_eq!(mine[0].peer.tagma_id, t2);
}

#[tokio::test]
async fn history_clamps_the_page_to_the_server_limit() {
    let (state, control) = db_state().await;
    let t1 = TagmaId::from("t-1".to_string());
    let t2 = TagmaId::from("t-2".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &t2, "alice", "tok-2");
    let view = create(&state, &t1, t2.as_ref()).await.expect("created");
    let db = state.require_db().expect("db").clone();
    // 201 rows straight through the store: the ROUTE's clamp is under test,
    // not the row volume.
    for i in 0..201u32 {
        direct_store::append(&db, view.session_id.as_ref(), t1.as_ref(), &[i as u8])
            .await
            .expect("append");
    }
    // A client asking for 1000 gets the server's 200-row page, from the start.
    let page = get_direct_session_messages(
        State(state),
        as_tagma(&t2),
        Path(view.session_id.as_ref().to_string()),
        axum::extract::Query(HistoryQuery {
            after_seq: None,
            limit: Some(1000),
        }),
    )
    .await
    .expect("history");
    assert_eq!(page.len(), 200);
    assert_eq!(page[0].seq, 1);
    assert_eq!(page[199].seq, 200);
}

#[tokio::test]
async fn history_degrades_handles_when_the_registry_blips() {
    let (state, control) = db_state().await;
    let t1 = TagmaId::from("t-1".to_string());
    let t2 = TagmaId::from("t-2".to_string());
    enroll(&control, &t1, "alice", "tok-1");
    enroll(&control, &t2, "alice", "tok-2");
    let view = create(&state, &t1, t2.as_ref()).await.expect("created");
    post_direct_session_envelope(
        State(state.clone()),
        as_tagma(&t1),
        Path(view.session_id.as_ref().to_string()),
        Json(envelope_text(&t1, &view.session_id, b"hello")),
    )
    .await
    .expect("accepted");
    // Inject a registry outage: the batched resolve the history read depends
    // on fails wholesale.
    control.set_tagma_profiles_failure(true);
    let page = get_direct_session_messages(
        State(state),
        as_tagma(&t2),
        Path(view.session_id.as_ref().to_string()),
        axum::extract::Query(HistoryQuery::default()),
    )
    .await
    .expect("history still served");
    // The pull is never blanked: the row comes back, its sender degraded to
    // the unforgeable id-prefix handle with no deep link.
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].seq, 1);
    assert_eq!(
        page[0].sender.handle,
        degraded_handle(
            &MemberId::from(ParticipantId::for_tagma(&t1)),
            ParticipantKind::Agent,
        )
    );
    assert_eq!(page[0].sender.tagma_id, None);
}
