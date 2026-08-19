//! Room envelope: a member's envelope is fanned to the other members; a
//! non-member sender is 404; an unknown room is 404.

use super::*;
use crate::auth::AuthPrincipal;
use crate::routes::test_support::{db_state, seed_room};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{
    ChannelId, ParticipantId, ParticipantKind, TagmaId, TraceId, UserId,
};
use kallip_agora_common::principal::Principal;
use kallip_lesche_common::event::LescheEvent;
use kallip_lesche_common::tunnel::TunnelInbound;
use time::OffsetDateTime;

fn envelope(sender: Participant, room: &str) -> Envelope {
    Envelope {
        channel_id: ChannelId::from(room.to_string()),
        sender,
        sequence_n: 1,
        trace_id: TraceId::from("t".to_string()),
        timestamp: OffsetDateTime::now_utc(),
        ciphertext: Ciphertext(vec![1u8; 12]),
    }
}

fn human(handle: &str, user: &str) -> Participant {
    Participant {
        id: ParticipantId::for_user(&UserId::from(user.to_string())),
        kind: ParticipantKind::Human,
        handle: handle.to_string(),
        tagma_id: None,
    }
}

fn agent(handle: &str, tagma: &TagmaId) -> Participant {
    Participant {
        id: ParticipantId::for_tagma(tagma),
        kind: ParticipantKind::Agent,
        handle: handle.to_string(),
        tagma_id: None,
    }
}

fn uid(s: &str) -> UserId {
    UserId::from(s.to_string())
}

/// Open `user`'s app stream and return a receiver (kept alive by the
/// caller) so a fan send lands on a live subscriber.
fn app_rx(state: &SharedConvState, user: &UserId) -> tokio::sync::broadcast::Receiver<LescheEvent> {
    let mut reg = state.write().unwrap();
    reg.open_app_stream(user).subscribe()
}

/// Register a live tagma tunnel and return its receiver.
fn tunnel_rx(
    state: &SharedConvState,
    tagma: &TagmaId,
    owner: &UserId,
) -> tokio::sync::broadcast::Receiver<TunnelInbound> {
    let mut reg = state.registry.write().unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel(8);
    let rx = tx.subscribe();
    reg.register_presence(tagma, owner.clone(), tx, std::sync::Arc::new(()));
    rx
}

fn participant(s: &str) -> AuthPrincipal {
    AuthPrincipal(Principal::User(uid(s)))
}

/// The verified-session display the AuthPrincipal extractor would stash for
/// a cookie-authed `s` user (username = `s`, no display name).
fn opt_user(s: &str) -> OptUserDisplay {
    OptUserDisplay(Some(crate::auth::UserDisplay {
        username: s.to_string(),
        display_name: None,
    }))
}

#[tokio::test]
async fn member_envelope_is_fanned_to_other_members() {
    let (state, _control) = db_state().await;
    let alice = uid("alice");
    let bob = uid("bob");
    let t1 = TagmaId::from("t1".to_string());
    seed_room(
        state.db.as_ref().unwrap(),
        "room-1",
        &alice,
        &[&bob],
        &[&t1],
    )
    .await;

    let mut bobs_rx = app_rx(&state, &bob);
    let mut t1_rx = tunnel_rx(&state, &t1, &alice);

    let env = envelope(human("Alice", "alice"), "room-1");
    let status = post_room_envelope(
        State(state),
        participant("alice"),
        opt_user("alice"),
        Path("room-1".to_string()),
        Json(env),
    )
    .await
    .expect("accepted");
    assert_eq!(status, StatusCode::ACCEPTED);

    // bob's app stream and t1's tunnel each received the envelope.
    let bob_ev = bobs_rx.recv().await.expect("bob received the envelope");
    let _ = t1_rx.recv().await;

    // Security: the relay stamps the authoritative STABLE handle, never the
    // client-supplied one. The sender sent handle "Alice"; the fan must
    // carry "@alice" (the stable username handle), proving a client cannot
    // spoof a display handle into the room.
    let kallip_lesche_common::event::LescheEvent::Envelope { envelope } = bob_ev else {
        panic!("expected an Envelope event");
    };
    assert_eq!(
        envelope.sender.handle, "@alice",
        "relay overwrites the client-supplied handle with the stable @username"
    );
}

#[tokio::test]
async fn agent_envelope_is_stamped_with_stable_owner_handle() {
    let (state, control) = db_state().await;
    let alice = uid("alice");
    let t1 = TagmaId::from("t1".to_string());
    // Enroll t1 so its identity (owner username) resolves at send time. The
    // owner's username is "alice" (MockControlPlane seeds it from the id).
    control.enroll_tagma(
        &t1,
        alice.clone(),
        kallip_agora_common::bytes::Ed25519PublicKey(vec![1u8; 32]),
        "tagma-token",
    );
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;

    // The owner's app stream receives the fanned envelope.
    let mut alice_rx = app_rx(&state, &alice);

    // The tagma sends with a SPOOFED handle "Evil"; the relay must overwrite
    // it with the stable `<id-prefix>@<owner>` handle.
    let env = envelope(agent("Evil", &t1), "room-1");
    let _ = post_room_envelope(
        State(state),
        AuthPrincipal(Principal::Tagma(t1.clone())),
        OptUserDisplay(None),
        Path("room-1".to_string()),
        Json(env),
    )
    .await
    .expect("accepted");

    let ev = alice_rx.recv().await.expect("alice received the envelope");
    let LescheEvent::Envelope { envelope } = ev else {
        panic!("expected an Envelope event");
    };
    let pid = ParticipantId::for_tagma(&t1);
    let prefix = &pid.as_ref()[..6];
    assert_eq!(
        envelope.sender.handle,
        format!("{}@alice", prefix),
        "agent handle is the stable id-prefix@owner"
    );
    assert_ne!(
        envelope.sender.handle, "Evil",
        "a tagma-supplied handle must not survive relay stamping"
    );
    // The relay also stamps the agent's tagma_id on the live envelope so a
    // message header can deep-link to the tagma profile.
    assert_eq!(
        envelope.sender.tagma_id,
        Some(t1.clone()),
        "agent sender carries its tagma_id"
    );
}

/// A not-usable sender tagma (revoked, or pending with no pinned key) on a
/// cache miss degrades to the unforgeable `agent <prefix>` handle -- never
/// the tagma-supplied handle, and the send still succeeds (no 500).
#[tokio::test]
async fn agent_envelope_degrades_to_prefix_when_not_usable() {
    let (state, control) = db_state().await;
    let alice = uid("alice");
    let revoked = TagmaId::from("rev".to_string());
    control.enroll_tagma(
        &revoked,
        alice.clone(),
        kallip_agora_common::bytes::Ed25519PublicKey(vec![1u8; 32]),
        "tok-rev",
    );
    control.revoke_tagma(&revoked);
    let pending = TagmaId::from("pen".to_string());
    control.enroll_tagma(
        &pending,
        alice.clone(),
        kallip_agora_common::bytes::Ed25519PublicKey(vec![2u8; 32]),
        "tok-pen",
    );
    control.set_pinned_key(&pending, None);
    seed_room(
        state.db.as_ref().unwrap(),
        "room-1",
        &alice,
        &[],
        &[&revoked, &pending],
    )
    .await;

    let mut alice_rx = app_rx(&state, &alice);

    for tagma in [&revoked, &pending] {
        let pid = ParticipantId::for_tagma(tagma);
        let prefix = pid.as_ref()[..6].to_string();
        let _ = post_room_envelope(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            OptUserDisplay(None),
            Path("room-1".to_string()),
            Json(envelope(agent("Evil", tagma), "room-1")),
        )
        .await
        .expect("accepted even when the sender is not usable");
        let ev = alice_rx.recv().await.expect("alice received the envelope");
        let LescheEvent::Envelope { envelope } = ev else {
            panic!("expected an Envelope event");
        };
        assert_eq!(
            envelope.sender.handle,
            format!("agent {}", prefix),
            "a not-usable sender degrades to the unforgeable id-prefix"
        );
    }
}

#[tokio::test]
async fn non_member_sender_is_404() {
    let (state, _control) = db_state().await;
    let alice = uid("alice");
    let t1 = TagmaId::from("t1".to_string());
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;
    // carol is not a member.
    let env = envelope(human("Carol", "carol"), "room-1");
    let err = post_room_envelope(
        State(state),
        participant("carol"),
        opt_user("carol"),
        Path("room-1".to_string()),
        Json(env),
    )
    .await
    .expect_err("non-member rejected");
    assert_eq!(err.status, 404);
}

/// Existence-oracle: a non-member cannot probe a room's existence by
/// spoofing another member's sender id. The membership gate runs BEFORE the
/// sender-match, so a non-member gets the same 404 as for an unknown room --
/// never a 403 that would confirm the room is real.
#[tokio::test]
async fn non_member_spoofed_sender_is_404_not_403() {
    let (state, _control) = db_state().await;
    let alice = uid("alice");
    let t1 = TagmaId::from("t1".to_string());
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;
    // carol is not a member, but spoofs alice's sender id on the envelope.
    let env = envelope(human("Alice", "alice"), "room-1");
    let err = post_room_envelope(
        State(state),
        participant("carol"),
        opt_user("carol"),
        Path("room-1".to_string()),
        Json(env),
    )
    .await
    .expect_err("non-member rejected even with a spoofed sender");
    assert_eq!(err.status, 404);
}

/// A member may send only as themselves: alice (a member) spoofing bob's
/// sender id is rejected. This locks the post-gate sender-match so a future
/// reorder cannot silently drop enforcement while the non-member oracle test
/// above still passes.
#[tokio::test]
async fn member_spoofing_another_member_is_403() {
    let (state, _control) = db_state().await;
    let alice = uid("alice");
    let bob = uid("bob");
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[&bob], &[]).await;
    // alice is a member but spoofs bob's sender id.
    let env = envelope(human("Bob", "bob"), "room-1");
    let err = post_room_envelope(
        State(state),
        participant("alice"),
        opt_user("alice"),
        Path("room-1".to_string()),
        Json(env),
    )
    .await
    .expect_err("member spoofing another member rejected");
    assert_eq!(err.status, 403);
}

#[tokio::test]
async fn unknown_room_is_404() {
    let (state, _control) = db_state().await;
    let env = envelope(human("Alice", "alice"), "ghost");
    let err = post_room_envelope(
        State(state),
        participant("alice"),
        opt_user("alice"),
        Path("ghost".to_string()),
        Json(env),
    )
    .await
    .expect_err("unknown room");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn history_member_empty_and_non_member_is_404() {
    let (state, _control) = db_state().await;
    let alice = uid("alice");
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[]).await;
    // Member: 200 with empty history.
    let Json(rows) = room_history(
        State(state.clone()),
        participant("alice"),
        Path("room-1".to_string()),
        Query(HistoryQuery::default()),
    )
    .await
    .expect("member history");
    assert!(rows.is_empty());

    // Non-member: 404.
    let err = room_history(
        State(state),
        participant("carol"),
        Path("room-1".to_string()),
        Query(HistoryQuery::default()),
    )
    .await
    .expect_err("non-member rejected");
    assert_eq!(err.status, 404);
}

/// Simulate a member leaving: hard-delete from `room_members` + append
/// to the `room_member_revocations` audit (the live/audit split a real
/// removal performs). Used to prove history still resolves a departed
/// sender's handle from the audit, not the gone live row.
async fn remove_member(
    db: &crate::db::Db,
    room: &str,
    pid: &ParticipantId,
    kind: ParticipantKind,
    source_id: &str,
) {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "DELETE FROM room_members WHERE room_id = $1 AND member_id = $2",
        [room.into(), pid.as_ref().to_string().into()],
    ))
    .await
    .expect("delete live member");
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO room_member_revocations \
             (id, room_id, member_id, kind, source_id, revoked_by, revoked_at, reason) \
             VALUES ($1::uuid, $2, $3, $4, $5, 'revoker', NOW(), 'test')",
        [
            uuid::Uuid::new_v4().to_string().into(),
            room.into(),
            pid.as_ref().to_string().into(),
            kind.as_str().into(),
            source_id.into(),
        ],
    ))
    .await
    .expect("insert revocation audit");
}

/// History read resolves each sender's display handle FRESH from the
/// registry -- the row stores only the stable `ParticipantId`, so the
/// handle matches the roster and is never a stale send-time snapshot. The
/// client-supplied handles ("Alice" / "Evil") do not survive; the stable
/// `@username` / `<prefix>@owner` forms are derived at read.
#[tokio::test]
async fn history_resolves_sender_handles_from_registry() {
    let (state, control) = db_state().await;
    let alice = uid("alice");
    let t1 = TagmaId::from("t1".to_string());
    // Enrolling t1 owned by alice also seeds alice's user identity
    // (username "alice"), which the human resolve needs.
    control.enroll_tagma(
        &t1,
        alice.clone(),
        kallip_agora_common::bytes::Ed25519PublicKey(vec![1u8; 32]),
        "tagma-token",
    );
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;

    // alice (human) sends with a spoofed handle, then t1 (agent) sends with
    // a spoofed handle.
    let _ = post_room_envelope(
        State(state.clone()),
        participant("alice"),
        opt_user("alice"),
        Path("room-1".to_string()),
        Json(envelope(human("Alice", "alice"), "room-1")),
    )
    .await
    .expect("alice send");
    let _ = post_room_envelope(
        State(state.clone()),
        AuthPrincipal(Principal::Tagma(t1.clone())),
        OptUserDisplay(None),
        Path("room-1".to_string()),
        Json(envelope(agent("Evil", &t1), "room-1")),
    )
    .await
    .expect("agent send");

    // Pull history as alice (a member).
    let Json(rows) = room_history(
        State(state),
        participant("alice"),
        Path("room-1".to_string()),
        Query(HistoryQuery::default()),
    )
    .await
    .expect("history");
    assert_eq!(rows.len(), 2);
    // Human sender: stable @username, NOT the client-supplied "Alice".
    assert_eq!(rows[0].sender.handle, "@alice");
    assert_eq!(rows[0].sender.kind, ParticipantKind::Human);
    assert_eq!(
        rows[0].sender.tagma_id, None,
        "human sender has no tagma_id"
    );
    // Agent sender: stable <id-prefix>@owner, NOT the spoofed "Evil".
    let prefix = ParticipantId::for_tagma(&t1).as_ref()[..6].to_string();
    assert_eq!(rows[1].sender.handle, format!("{}@alice", prefix));
    assert_eq!(rows[1].sender.kind, ParticipantKind::Agent);
    assert_eq!(
        rows[1].sender.tagma_id,
        Some(t1.clone()),
        "agent sender carries its tagma_id"
    );
}

/// A departed sender is gone from the live membership but retained in the
/// `room_member_revocations` audit; history read resolves the real
/// `@owner` handle via the audit rather than degrading to a bare prefix.
#[tokio::test]
async fn history_resolves_a_departed_sender_via_revocations() {
    let (state, control) = db_state().await;
    let alice = uid("alice");
    let bob = uid("bob");
    // Seed bob's identity so the registry resolves him to "@bob" after he
    // leaves (he is no tagma owner, so enroll_tagma does not seed him).
    control.seed_user(bob.clone());
    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[&bob], &[]).await;

    // bob sends, then leaves the room.
    let bob_pid = ParticipantId::for_user(&bob);
    let _ = post_room_envelope(
        State(state.clone()),
        participant("bob"),
        opt_user("bob"),
        Path("room-1".to_string()),
        Json(envelope(human("Bob", "bob"), "room-1")),
    )
    .await
    .expect("bob send");
    remove_member(
        state.db.as_ref().unwrap(),
        "room-1",
        &bob_pid,
        ParticipantKind::Human,
        "bob",
    )
    .await;

    // alice pulls history: bob is gone from the live membership, but his
    // message remains and its handle resolves via the revocations audit.
    let Json(rows) = room_history(
        State(state),
        participant("alice"),
        Path("room-1".to_string()),
        Query(HistoryQuery::default()),
    )
    .await
    .expect("history");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].sender.handle, "@bob",
        "departed sender resolved via revocations, not degraded to a prefix"
    );
}
