//! Tests for the lesche faces: the request guards of each send target, the two
//! reads' query handling, the session list, and the relay stubs they run
//! against.

use super::direct::{
    DirectHistoryQuery, read_direct_session_messages, read_direct_session_messages_by_id,
};
use super::room::{LescheHistoryQuery, list_joined_rooms, read_room_messages};
use super::send::{LescheMessageRequest, post_message, send_room_message};
use super::sessions::list_lesche_sessions;

use axum::Json;
use axum::extract::{Path, Query, State};
use kallipai_archeion_common::ids::TagmaId;
use kallipai_common::agentid::AgentId;
use kallipai_common::message::DeliveryResponse;
use kallipai_common::protocol::ApiError;
use kallipai_lesche_common::direct::{DirectMessage, DirectSessionId};
use kallipai_lesche_common::message::{Envelope, RoomMessage};
use kallipai_lesche_common::rooms::{MemberId, RoomId};

use crate::auth::{AuthIdentity, Identity};
use crate::external::ExternalProjector;
use crate::relay::MessageLimits;
use crate::state::{RegistryEntry, SharedState};
use crate::test_helpers::{make_entry, make_state};

/// With neither the relay nor the direct serving path initialized (a state
/// only constructed in tests — production always inits direct), the route
/// returns 503 rather than touching either serving path. Authed as the
/// agent itself (self-only) so the 403 check does not short-circuit.
#[tokio::test]
async fn message_unavailable_when_no_relay() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: None,
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("no relay -> unavailable");
    assert_eq!(
        err.status, 503,
        "expected 503 unavailable, got {}",
        err.status
    );
}

/// A room send (`--room <room>`) takes the room branch, which needs the
/// external projector (for the shared burst cap) -- absent here, so it
/// short-circuits to 503. Proves the branch is taken and routes through
/// `send_room_message` rather than the bilateral projector path.
#[tokio::test]
async fn room_send_unavailable_without_projector() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: Some("room-1".into()),
            tagma: None,
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("no projector -> unavailable");
    assert_eq!(
        err.status, 503,
        "expected 503 unavailable, got {}",
        err.status
    );
}

/// `list_joined_rooms` with an empty joined-rooms cache returns an empty
/// list rather than 503: the room list reads the cache directly and needs no
/// other state. Pins that the route is wired through `require_root_self` and
/// tolerates a cold cache.
#[tokio::test]
async fn list_joined_rooms_with_cold_cache_returns_empty() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let Json(rooms) = list_joined_rooms(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
    )
    .await
    .expect("cold cache -> empty list, not 503");
    assert!(rooms.is_empty());
}

/// `list_joined_rooms` rejects a subagent (the shared `require_root_self`
/// guard). The guard now serves three routes; this pins that the extraction
/// did not regress the 403 path for the room-discovery route.
#[tokio::test]
async fn list_joined_rooms_forbidden_for_subagent() {
    let state = make_state();
    let root = AgentId::random();
    let sub = AgentId::random();
    {
        let mut registry = state.registry.write().await;
        registry.register(
            root.clone(),
            RegistryEntry::Live(make_entry(None, "root".into())),
        );
        registry.register(
            sub.clone(),
            RegistryEntry::Live(make_entry(Some(root), "sub".into())),
        );
    }

    let err = list_joined_rooms(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: sub.clone() }),
        Path(sub),
    )
    .await
    .expect_err("subagent -> forbidden");
    assert_eq!(err.status, 403);
}

/// The operator may NOT send as an agent — a message is something the end
/// user attributes to the agent, so an operator posting one would forge the
/// agent's voice. This is the deliberate narrowing from
/// `require_self_or_operator` (used by non-impersonating self-writes) to
/// `require_self`.
#[tokio::test]
async fn message_forbidden_for_operator() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: None,
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("operator -> forbidden");
    assert_eq!(
        err.status, 403,
        "expected 403 forbidden, got {}",
        err.status
    );
}

/// A peer agent may not send for another agent either (self-only).
#[tokio::test]
async fn message_forbidden_for_other_agent() {
    let state = make_state();
    let a = AgentId::random();
    let b = AgentId::random();
    let entry_a = make_entry(None, "a".to_string());
    let entry_b = make_entry(Some(a.clone()), "b".to_string());
    {
        let mut registry = state.registry.write().await;
        registry.register(a.clone(), RegistryEntry::Live(entry_a));
        registry.register(b.clone(), RegistryEntry::Live(entry_b));
    }

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: a }),
        Path(b),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: None,
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("peer agent -> forbidden");
    assert_eq!(
        err.status, 403,
        "expected 403 forbidden, got {}",
        err.status
    );
}

/// A subagent may not deliver a user-facing message even when posting as
/// itself (self-only passes) — the conversation with the user is owned by
/// the root, so a subagent must route outward communication through its
/// supervisor.
#[tokio::test]
async fn message_forbidden_for_subagent() {
    let state = make_state();
    let root = AgentId::random();
    let sub = AgentId::random();
    {
        let mut registry = state.registry.write().await;
        registry.register(
            root.clone(),
            RegistryEntry::Live(make_entry(None, "root".into())),
        );
        registry.register(
            sub.clone(),
            RegistryEntry::Live(make_entry(Some(root), "sub".into())),
        );
    }

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: sub.clone() }),
        Path(sub),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: None,
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("subagent -> forbidden");
    assert_eq!(
        err.status, 403,
        "expected 403 forbidden, got {}",
        err.status
    );
}

/// A send naming BOTH targets (room + tagma) is a caller bug -> 400,
/// before any projector/relay work. Pins the three-addressing dispatch
/// invariant that the two optional targets are mutually exclusive.
#[tokio::test]
async fn direct_send_rejects_both_targets() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: Some("room-1".into()),
            tagma: Some("tagma-b".into()),
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("both targets -> bad request");
    assert_eq!(err.status, 400, "expected 400, got {}", err.status);
}

/// A direct send takes the tagma branch before any relay work: without
/// the external projector (the shared burst-cap owner) it short-circuits
/// to 503, mirroring the room branch. Pins that `--tagma` routes through
/// `send_direct_message` rather than the bilateral projector path.
#[tokio::test]
async fn direct_send_unavailable_without_projector() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: Some("tagma-b".into()),
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("no projector -> unavailable");
    assert_eq!(
        err.status, 503,
        "expected 503 unavailable, got {}",
        err.status
    );
}

/// The direct-session history read with no relay installed is 503 -- the
/// derivation needs the enrolled tagma id, which only a relay carries.
#[tokio::test]
async fn direct_read_unavailable_without_relay() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = read_direct_session_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "tagma-b".to_string())),
        Query(DirectHistoryQuery {
            after_seq: None,
            limit: None,
            format: None,
        }),
    )
    .await
    .expect_err("no relay -> unavailable");
    assert_eq!(
        err.status, 503,
        "expected 503 unavailable, got {}",
        err.status
    );
}

/// The format is the query's own to get wrong, so a malformed one is 400
/// even with no relay installed: a 503 would blame a party that has not
/// been reached yet.
#[tokio::test]
async fn a_bad_format_is_refused_before_the_relay_lookup() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = read_direct_session_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "tagma-b".to_string())),
        Query(DirectHistoryQuery {
            after_seq: None,
            limit: None,
            format: Some("xml".to_string()),
        }),
    )
    .await
    .expect_err("a format the route does not serve is 400");

    assert_eq!(
        err.status, 400,
        "a bad format is the request's fault, not a missing relay: {}",
        err.status
    );
}

/// The session-addressed route answers the same way: which route spelled
/// the log does not change who got the format wrong.
#[tokio::test]
async fn a_bad_format_is_refused_on_the_session_addressed_read_too() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = read_direct_session_messages_by_id(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "00000000-0000-0000-0000-0000000000a1".to_string())),
        Query(DirectHistoryQuery {
            after_seq: None,
            limit: None,
            format: Some("xml".to_string()),
        }),
    )
    .await
    .expect_err("a format the route does not serve is 400");

    assert_eq!(err.status, 400, "bad request, not a missing relay");
}

/// The session-addressed read is handed an id somebody spelled, so its
/// shape is this request's own: a value that cannot name a session is
/// refused as a bad request rather than carried to a member-set lookup
/// that could only say no to it.
#[tokio::test]
async fn a_session_id_that_is_not_canonical_is_refused() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = read_direct_session_messages_by_id(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "../../agents".to_string())),
        Query(DirectHistoryQuery {
            after_seq: None,
            limit: None,
            format: None,
        }),
    )
    .await
    .expect_err("a value that cannot name a session is this request's mistake");

    assert_eq!(err.status, 400, "bad request, not an unavailable relay");
}

/// A position is an ordinal in the log, so a negative one is no direction
/// at all. Both reads answer it here, on either cursor: the lesche reads a
/// negative one as "from the start", and this hop would surface that as an
/// upstream fault rather than the caller's mistake.
#[tokio::test]
async fn a_negative_position_is_refused_by_both_reads() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let direct = read_direct_session_messages(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id.clone(), "tagma-b".to_string())),
        Query(DirectHistoryQuery {
            after_seq: Some(-1),
            limit: None,
            format: None,
        }),
    )
    .await
    .expect_err("a negative position is not a page direction");
    assert_eq!(direct.status, 400, "bad request, not an unavailable relay");

    for query in [
        LescheHistoryQuery {
            after_seq: Some(-1),
            before_seq: None,
            thread: None,
            limit: None,
        },
        LescheHistoryQuery {
            after_seq: None,
            before_seq: Some(-1),
            thread: None,
            limit: None,
        },
    ] {
        let room = read_room_messages(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((
                id.clone(),
                "00000000-0000-0000-0000-000000000aa1".to_string(),
            )),
            Query(query),
        )
        .await
        .expect_err("a negative position is not a page direction");
        assert_eq!(room.status, 400, "bad request, not an unavailable relay");
    }
}

/// A discussion root is a seq like any other, so below 1 it names nothing; and
/// a discussion is read forward only, so a backward cursor beside it is a page
/// this hop cannot serve. Both are the caller's mistake, refused here instead
/// of being smoothed over into an upstream fault.
#[tokio::test]
async fn a_thread_position_must_be_positive_and_forward_only() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    for query in [
        LescheHistoryQuery {
            after_seq: None,
            before_seq: None,
            thread: Some(0),
            limit: None,
        },
        LescheHistoryQuery {
            after_seq: None,
            before_seq: Some(8),
            thread: Some(1),
            limit: None,
        },
    ] {
        let room = read_room_messages(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((
                id.clone(),
                "00000000-0000-0000-0000-000000000aa1".to_string(),
            )),
            Query(query),
        )
        .await
        .expect_err("a discussion root below 1 or beside a backward cursor is no page");
        assert_eq!(room.status, 400, "bad request, not an unavailable relay");
    }
}

/// The unified session list with no relays and no projector returns an
/// empty list rather than 503: every member of the aggregate is
/// best-effort, mirroring the cold-cache room list.
#[tokio::test]
async fn sessions_with_cold_state_returns_empty() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let Json(entries) = list_lesche_sessions(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
    )
    .await
    .expect("cold state -> empty list, not 503");
    assert!(entries.is_empty());
}

/// The console reads the session list through the manage bridge, which
/// authenticates as the operator: the read routes accept the operator
/// identity (read-side narrowing of `require_root_self`), so a cold
/// state yields the same empty list it yields for the agent.
#[tokio::test]
async fn sessions_readable_by_operator() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let Json(entries) = list_lesche_sessions(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(id),
    )
    .await
    .expect("operator reads the session list");
    assert!(entries.is_empty());
}

/// The operator passes the direct-history read guard and reaches the
/// relay-dependent leg: with no relay installed the route fails with
/// 503 (unavailable), NOT 403 -- pinning that the guard relaxation
/// covers the bridge identity the console actually presents.
#[tokio::test]
async fn direct_read_passes_guard_for_operator() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = read_direct_session_messages(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path((id, "tagma-b".to_string())),
        Query(DirectHistoryQuery {
            after_seq: None,
            limit: None,
            format: None,
        }),
    )
    .await
    .expect_err("no relay -> 503, guard passed");
    assert_eq!(
        err.status, 503,
        "expected 503 unavailable, got {}",
        err.status
    );
}

/// The room face keeps the strict guard: an operator authenticated by the
/// manage bridge gets 403 on both the address-book list and the history
/// read, the same answer the send route gives. The bridge mounts neither
/// room route, so this pins that the read-side widening did not reach here.
/// The entry registered here is the root (`created_by: None`), so a widened
/// guard would let the operator through and this test would fail.
#[tokio::test]
async fn room_reads_forbidden_for_operator() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let err = list_joined_rooms(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(id.clone()),
    )
    .await
    .expect_err("operator -> forbidden on the room list");
    assert_eq!(
        err.status, 403,
        "expected 403 forbidden, got {}",
        err.status
    );

    let err = read_room_messages(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path((id, "room-a".to_string())),
        Query(LescheHistoryQuery {
            after_seq: None,
            before_seq: None,
            thread: None,
            limit: None,
        }),
    )
    .await
    .expect_err("operator -> forbidden on the room history");
    assert_eq!(
        err.status, 403,
        "expected 403 forbidden, got {}",
        err.status
    );
}

/// The direct-history read hands the data plane the caller's own cursor and
/// page size, both of them: the route renders whatever the lesche answers,
/// so a dropped `limit` would print a page nobody asked for.
#[tokio::test]
async fn direct_read_passes_its_cursor_and_page_size_on() {
    let (state, id, mock) = setup_lesche_send(Vec::new()).await;
    let own = TagmaId::from("t1".to_string());
    let peer = TagmaId::from("tagma-peer".to_string());
    state
        .direct_sessions
        .set_direct_sessions("test", [DirectSessionId::for_pair(&own, &peer)])
        .await;

    let _page = read_direct_session_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "tagma-peer".to_string())),
        axum::extract::Query(DirectHistoryQuery {
            after_seq: Some(41),
            limit: Some(1),
            format: None,
        }),
    )
    .await
    .expect("the read reaches the lesche");

    let reads = mock.direct_reads.lock().unwrap().clone();
    assert_eq!(reads.len(), 1, "one history GET");
    let query = reads[0].as_str();
    assert!(query.contains("after_seq=41"), "{query}");
    assert!(query.contains("limit=1"), "{query}");
}

/// The session-addressed read fetches the session the path names: no peer
/// pair is derived, so the log a delivered message's read-back command
/// spells is the log this read asks the lesche for.
#[tokio::test]
async fn a_session_addressed_read_fetches_that_session() {
    let (state, id, mock) = setup_lesche_send(Vec::new()).await;
    let own = TagmaId::from("t1".to_string());
    let peer = TagmaId::from("tagma-peer".to_string());
    let session = DirectSessionId::for_pair(&own, &peer);
    state
        .direct_sessions
        .set_direct_sessions("test", [session.clone()])
        .await;

    let _page = read_direct_session_messages_by_id(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, session.to_string())),
        axum::extract::Query(DirectHistoryQuery {
            after_seq: Some(6),
            limit: Some(1),
            format: None,
        }),
    )
    .await
    .expect("the read reaches the lesche");

    let reads = mock.direct_reads.lock().unwrap().clone();
    assert_eq!(reads.len(), 1, "one history GET");
    let read = reads[0].as_str();
    assert!(
        read.contains(&format!("/direct-sessions/{session}/messages")),
        "the path names the session itself: {read}"
    );
    assert!(
        read.contains("after_seq=6"),
        "the cursor rides along: {read}"
    );
    assert!(
        read.contains("limit=1"),
        "the page size rides along: {read}"
    );
}

/// The direct-send branch shares the per-tagma burst cap with the
/// bilateral path: a projector configured with a zero-size budget makes
/// the very first `--tagma` send a 429 before any relay is contacted
/// (the create-or-get never runs). Pins the shared-cap wiring the
/// direct branch relies on.
#[tokio::test]
async fn direct_send_hits_the_burst_cap_before_any_relay_contact() {
    let state = make_state();
    let id = AgentId::random();
    let entry = make_entry(None, "tok".to_string());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    state
        .external
        .set(ExternalProjector::new(
            std::sync::Arc::downgrade(&state),
            None,
            None,
            None,
            None,
            MessageLimits {
                max: 0,
                window: std::time::Duration::from_secs(60),
            },
        ))
        .ok()
        .expect("projector set once");

    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: Some("tagma-peer".into()),
            attachment: None,
            quote: None,
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("max:0 budget -> 429");
    assert_eq!(err.status, 429, "expected 429, got {}", err.status);
}

/// Every envelope body the stub lesche received, in arrival order.
type PostedBodies = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// The query string of every direct-history read the stub lesche answered.
type ReadQueries = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// The room message inside one posted envelope: what actually crossed the
/// wire, once the envelope's plaintext payload is decoded.
fn wire_room_message(body: &[u8]) -> RoomMessage {
    let envelope: Envelope = serde_json::from_slice(body).expect("envelope json");
    serde_json::from_slice(&envelope.ciphertext.0).expect("room message json")
}

/// A room send reaches the lesche with the quote on the wire, and a
/// non-positive quote reaches it with no quote field at all.
#[tokio::test]
async fn room_send_carries_the_quote_to_the_lesche() {
    let (state, id, mock) = setup_lesche_send(Vec::new()).await;
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    assert!(
        send_shaped(
            state.clone(),
            id.clone(),
            Some("room-1"),
            None,
            Vec::new(),
            Some(7),
            None,
        )
        .await
        .expect("quoted room send")
        .0
        .ok
    );
    assert!(
        send_shaped(state, id, Some("room-1"), None, Vec::new(), Some(0), None)
            .await
            .expect("zero-quote room send")
            .0
            .ok
    );

    let bodies = mock.posted.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one envelope per send");
    let quoted = wire_room_message(&bodies[0]);
    assert_eq!(quoted.quote, Some(7), "the quote crosses the wire");
    assert_eq!(quoted.text, "ping");
    let zero = wire_room_message(&bodies[1]);
    assert_eq!(zero.quote, None, "a zero quote is no quote on the wire");
}

/// The thread the sender declares crosses the wire as its OWN field: the
/// payload's `quote` and `thread` are adjacent same-typed slots, so a swap
/// would compile, and only reading the payload back catches it.
#[tokio::test]
async fn room_send_carries_the_declared_thread_to_the_lesche() {
    let (state, id, mock) = setup_lesche_send(Vec::new()).await;
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    let sent = send_shaped(
        state,
        id,
        Some("room-1"),
        None,
        Vec::new(),
        Some(4),
        Some(2),
    )
    .await
    .expect("a room send that quotes a row and declares a thread");
    assert!(sent.0.ok, "the send is accepted");

    let bodies = mock.posted.lock().unwrap().clone();
    let sent = wire_room_message(&bodies[0]);
    assert_eq!(sent.quote, Some(4), "the quoted row crosses the wire");
    assert_eq!(
        sent.thread,
        Some(2),
        "the declared thread is its own field, not the quoted row"
    );
}

/// The route reads its request off JSON bytes, and the client's request is a
/// separate struct: a renamed field here would compile while the `thread` key
/// the client sends fell on the floor, so pin the wire key itself.
#[test]
fn the_request_reads_a_thread_from_the_wire_key() {
    let declared: LescheMessageRequest = serde_json::from_value(serde_json::json!({
        "text": "ping",
        "room": "ops",
        "thread": 3
    }))
    .expect("decodes");
    assert_eq!(declared.thread, Some(3), "the declared thread reads in");

    let absent: LescheMessageRequest = serde_json::from_value(serde_json::json!({
        "text": "ping",
        "room": "ops"
    }))
    .expect("decodes");
    assert_eq!(absent.thread, None, "a send that declares none stays none");
}

/// A direct-session send reaches the lesche with the quote on the wire, and
/// an unquoted or non-positive one keeps the payload shape a pre-quote
/// writer wrote.
#[tokio::test]
async fn direct_send_carries_the_quote_to_the_lesche() {
    let (state, id, mock) = setup_lesche_send(Vec::new()).await;

    assert!(
        send_shaped(
            state.clone(),
            id.clone(),
            None,
            Some("tagma-peer"),
            Vec::new(),
            Some(7),
            None,
        )
        .await
        .expect("quoted direct send")
        .0
        .ok
    );
    assert!(
        send_shaped(
            state.clone(),
            id.clone(),
            None,
            Some("tagma-peer"),
            Vec::new(),
            None,
            None,
        )
        .await
        .expect("plain direct send")
        .0
        .ok
    );
    assert!(
        send_shaped(
            state,
            id,
            None,
            Some("tagma-peer"),
            Vec::new(),
            Some(0),
            None
        )
        .await
        .expect("zero-quote direct send")
        .0
        .ok
    );

    let bodies = mock.direct_posted.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3, "one envelope per send");
    let quoted: Envelope = serde_json::from_slice(&bodies[0]).expect("envelope json");
    let payload: DirectMessage =
        serde_json::from_slice(&quoted.ciphertext.0).expect("direct message json");
    assert_eq!(payload.quote, Some(7), "the quote crosses the wire");
    assert_eq!(payload.text, "ping");
    let plain: Envelope = serde_json::from_slice(&bodies[1]).expect("envelope json");
    assert_eq!(
        plain.ciphertext.0, br#"{"text":"ping"}"#,
        "an unquoted direct payload keeps its pre-quote bytes"
    );
    let zero: Envelope = serde_json::from_slice(&bodies[2]).expect("envelope json");
    assert_eq!(
        zero.ciphertext.0, br#"{"text":"ping"}"#,
        "a zero quote is no quote on the wire"
    );
}

/// How many times the mock lesche was asked for the tagma's room list.
type RosterPulls = std::sync::Arc<std::sync::atomic::AtomicUsize>;

/// The mock lesche's shared state: the room and direct-session envelopes it
/// received, the query each direct-history read carried, the room list it
/// serves, how often it was asked for that list, and the status it answers
/// that list with (zero means 200, so a test that never sets it gets a
/// served roster; 500 makes the refresh fail).
#[derive(Clone, Default)]
struct LescheSendMock {
    posted: PostedBodies,
    direct_posted: PostedBodies,
    direct_reads: ReadQueries,
    pulls: RosterPulls,
    roster: std::sync::Arc<std::sync::Mutex<Vec<kallipai_lesche_common::rooms::TagmaRoomView>>>,
    roster_status: std::sync::Arc<std::sync::atomic::AtomicU16>,
}

/// A stand-in lesche serving the room-envelope route, the direct-session
/// create-or-get and message routes, and the tagma's own room list: a send
/// test can read what crossed the wire and tell a cached roster from a
/// refreshed one.
async fn spawn_lesche_send_stub(mock: LescheSendMock) -> String {
    use std::sync::atomic::Ordering;

    let app = axum::Router::new()
        .route(
            "/v1/lesche/rooms/{_room}/envelopes",
            axum::routing::post(
                |State(mock): State<LescheSendMock>, body: axum::body::Bytes| async move {
                    mock.posted.lock().unwrap().push(body.to_vec());
                    axum::http::StatusCode::ACCEPTED
                },
            ),
        )
        .route(
            "/v1/lesche/direct-sessions",
            axum::routing::post(|State(_mock): State<LescheSendMock>| async move {
                Json(serde_json::json!({
                    "session_id": "00000000-0000-4000-8000-0000000000dd",
                    "peer": { "tagma_id": "tagma-peer", "handle": "peer@alice" },
                    "created_at": "2026-01-01T00:00:00Z",
                }))
            }),
        )
        .route(
            "/v1/lesche/direct-sessions/{_session}/messages",
            axum::routing::get(
                |State(mock): State<LescheSendMock>, uri: axum::http::Uri| async move {
                    // The path is recorded with the query: a read is
                    // addressed by the session in its path, so a test that
                    // only saw the query could not tell the two direct-read
                    // routes apart.
                    let recorded = uri
                        .path_and_query()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_default();
                    mock.direct_reads.lock().unwrap().push(recorded);
                    Json(Vec::<serde_json::Value>::new())
                },
            )
            .post(
                |State(mock): State<LescheSendMock>, body: axum::body::Bytes| async move {
                    mock.direct_posted.lock().unwrap().push(body.to_vec());
                    axum::http::StatusCode::ACCEPTED
                },
            ),
        )
        .route(
            "/v1/lesche/tagmata/{_tagma}/rooms",
            axum::routing::get(|State(mock): State<LescheSendMock>| async move {
                mock.pulls.fetch_add(1, Ordering::SeqCst);
                let served = mock.roster_status.load(Ordering::SeqCst);
                let code =
                    axum::http::StatusCode::from_u16(served).unwrap_or(axum::http::StatusCode::OK);
                let rooms = if code == axum::http::StatusCode::OK {
                    mock.roster.lock().unwrap().clone()
                } else {
                    Vec::new()
                };
                (code, Json(rooms))
            }),
        )
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1/lesche")
}

/// A state wired to one live relay and the mock lesche, with the calling
/// agent registered (the send route is root-self). The room cache starts
/// cold: a test seeds it with ids or with a snapshot as its case needs.
async fn setup_lesche_send(
    roster: Vec<kallipai_lesche_common::rooms::TagmaRoomView>,
) -> (SharedState, AgentId, LescheSendMock) {
    let mock = LescheSendMock::default();
    *mock.roster.lock().unwrap() = roster;
    let lesche_url = spawn_lesche_send_stub(mock.clone()).await;
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );
    let projector = ExternalProjector::new(
        std::sync::Arc::downgrade(&state),
        None,
        None,
        None,
        None,
        MessageLimits::default(),
    );
    assert!(state.external.set(projector).is_ok(), "projector installed");
    let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
        .build()
        .expect("lesche client");
    let handle = crate::relay::RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("t1".to_string()),
        "Tagma".into(),
        kallipai_e2ee::DeviceKey::generate(),
        id.clone(),
        std::sync::Arc::downgrade(&state),
    );
    crate::test_helpers::install_relay(&state, "test", handle);
    (state, id, mock)
}

/// One send through the route, as the CLI's `lesche send` shapes it.
async fn send_with(
    state: SharedState,
    id: AgentId,
    room: Option<&str>,
    tagma: Option<&str>,
    mentions: Vec<String>,
) -> Result<Json<DeliveryResponse>, ApiError> {
    send_shaped(state, id, room, tagma, mentions, None, None).await
}

/// One send with every field the route reads spelled out: `send_with` for
/// the cases that vary only the target and the names, this one for a case
/// that also carries a quote.
async fn send_shaped(
    state: SharedState,
    id: AgentId,
    room: Option<&str>,
    tagma: Option<&str>,
    mentions: Vec<String>,
    quote: Option<i64>,
    thread: Option<i64>,
) -> Result<Json<DeliveryResponse>, ApiError> {
    post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "ping".into(),
            room: room.map(str::to_owned),
            tagma: tagma.map(str::to_owned),
            attachment: None,
            quote,
            mentions: Some(mentions),
            thread,
        }),
    )
    .await
}

fn member_id(name: &str) -> MemberId {
    MemberId::for_tagma(&kallipai_archeion_common::ids::TagmaId::from(
        name.to_string(),
    ))
}

fn room_view_with(
    room: &str,
    members: Vec<MemberId>,
) -> kallipai_lesche_common::rooms::TagmaRoomView {
    use kallipai_archeion_common::ids::ParticipantKind;
    use kallipai_lesche_common::rooms::{RoomMember, TagmaRoomView, Visibility};
    TagmaRoomView {
        room_id: RoomId::from(room.to_string()),
        members: members
            .into_iter()
            .map(|id| RoomMember {
                id,
                kind: ParticipantKind::Agent,
            })
            .collect(),
        membership_epoch: 1,
        is_creator: false,
        visibility: Visibility::Private,
        name: None,
    }
}

/// Mentions reach the lesche in the sender's order with repeats folded, an
/// unnamed send carries none, and a cached roster answers from the cache.
#[tokio::test]
async fn room_send_carries_mentions_to_the_lesche() {
    use std::sync::atomic::Ordering;

    let ada = member_id("ada");
    let bo = member_id("bo");
    let view = room_view_with("room-1", vec![ada.clone(), bo.clone()]);
    let (state, id, mock) = setup_lesche_send(vec![view.clone()]).await;
    state.joined_rooms.set_room_snapshot("test", &[view]).await;

    let named = vec![
        ada.as_ref().to_owned(),
        bo.as_ref().to_owned(),
        ada.as_ref().to_owned(),
    ];
    assert!(
        send_with(state.clone(), id.clone(), Some("room-1"), None, named)
            .await
            .expect("named room send")
            .0
            .ok
    );
    assert!(
        send_with(state.clone(), id.clone(), Some("room-1"), None, Vec::new())
            .await
            .expect("unnamed room send")
            .0
            .ok
    );

    let bodies = mock.posted.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one envelope per send");
    assert_eq!(
        wire_room_message(&bodies[0]).mentions,
        vec![ada.clone(), bo.clone()],
        "the members cross the wire in order, the repeat folded"
    );
    assert!(
        wire_room_message(&bodies[1]).mentions.is_empty(),
        "a send that named nobody names nobody on the wire"
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        0,
        "a cached roster costs no call to the lesche"
    );
}

/// A mention that is not a participant id is refused with the text the
/// caller typed, before the send reaches any room.
#[tokio::test]
async fn a_malformed_mention_is_refused() {
    let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
    let err = send_with(
        state,
        id,
        Some("room-1"),
        None,
        vec!["not-an-id".to_string()],
    )
    .await
    .expect_err("a malformed id names nobody");
    assert_eq!(err.status, 400);
    assert_eq!(err.message, "not a participant id: not-an-id");
}

/// The cap counts members after repeats fold: seventeen distinct ids are
/// refused, and the refusal states the cap.
#[tokio::test]
async fn naming_more_members_than_the_cap_is_refused() {
    let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
    let named: Vec<String> = (0..17)
        .map(|n| member_id(&format!("member-{n}")).as_ref().to_owned())
        .collect();
    let err = send_with(state, id, Some("room-1"), None, named)
        .await
        .expect_err("seventeen members is one over the cap");
    assert_eq!(err.status, 400);
    assert_eq!(err.message, "a send names more than 16 members");
}

/// A mention names a member of the room the send enters, so a send at
/// another target is refused rather than silently dropping the names.
#[tokio::test]
async fn a_mention_without_a_room_is_refused() {
    let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
    let err = send_with(
        state,
        id,
        None,
        Some("tagma-peer"),
        vec![member_id("ada").as_ref().to_owned()],
    )
    .await
    .expect_err("a direct-session send names no room");
    assert_eq!(err.status, 400);
    assert_eq!(
        err.message,
        "mentions ride a room send: a participant id names a member of that room"
    );
}

/// A member the fresh roster does not name is refused, and the refusal
/// names the id: the cached slice only sends the judge to the refresh, so
/// a refusal never rests on what that slice said.
#[tokio::test]
async fn a_mention_of_a_non_member_is_refused() {
    use std::sync::atomic::Ordering;

    let ada = member_id("ada");
    let bo = member_id("bo");
    let view = room_view_with("room-1", vec![ada.clone()]);
    let (state, id, mock) = setup_lesche_send(vec![view.clone()]).await;
    state.joined_rooms.set_room_snapshot("test", &[view]).await;

    let err = send_with(
        state,
        id,
        Some("room-1"),
        None,
        vec![bo.as_ref().to_owned()],
    )
    .await
    .expect_err("bo is not in this room");
    assert_eq!(err.status, 400);
    assert_eq!(
        err.message,
        format!("mention {} is not a member of this room", bo.as_ref())
    );
    assert_eq!(
        mock.posted.lock().unwrap().len(),
        0,
        "a refused mention sends nothing"
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        1,
        "a slice that lacks the id sends the judge to the refresh"
    );
}

/// A roster the poll has not cached is refreshed once, and the refreshed
/// roster is what the next send is judged against: a member it names goes
/// through, one it does not is refused.
#[tokio::test]
async fn a_cold_roster_is_refreshed_before_a_mention_is_judged() {
    use std::sync::atomic::Ordering;

    let ada = member_id("ada");
    let bo = member_id("bo");
    let (state, id, mock) =
        setup_lesche_send(vec![room_view_with("room-1", vec![ada.clone()])]).await;
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    assert!(
        send_with(
            state.clone(),
            id.clone(),
            Some("room-1"),
            None,
            vec![ada.as_ref().to_owned()],
        )
        .await
        .expect("the refreshed roster names the member")
        .0
        .ok
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        1,
        "a cold roster is refreshed once"
    );

    assert!(
        send_with(
            state.clone(),
            id.clone(),
            Some("room-1"),
            None,
            vec![ada.as_ref().to_owned()],
        )
        .await
        .expect("the refreshed roster is cached")
        .0
        .ok
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        1,
        "the refresh warmed the cache for the next send"
    );

    let err = send_with(
        state,
        id,
        Some("room-1"),
        None,
        vec![bo.as_ref().to_owned()],
    )
    .await
    .expect_err("the refreshed roster does not name bo");
    assert_eq!(err.status, 400);
    assert_eq!(
        err.message,
        format!("mention {} is not a member of this room", bo.as_ref())
    );
}

/// A cached roster the poll has not caught up with does not refuse a
/// member the lesche names: the refresh answers, and the send goes through.
#[tokio::test]
async fn a_member_the_fresh_roster_names_is_let_through() {
    use std::sync::atomic::Ordering;

    let ada = member_id("ada");
    let bo = member_id("bo");
    let stale = room_view_with("room-1", vec![ada.clone()]);
    let fresh = room_view_with("room-1", vec![ada, bo.clone()]);
    let (state, id, mock) = setup_lesche_send(vec![fresh]).await;
    state.joined_rooms.set_room_snapshot("test", &[stale]).await;

    assert!(
        send_with(
            state,
            id,
            Some("room-1"),
            None,
            vec![bo.as_ref().to_owned()],
        )
        .await
        .expect("the fresh roster names the member")
        .0
        .ok
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        1,
        "the cached slice sends the judge to the refresh"
    );
    assert_eq!(
        mock.posted.lock().unwrap().len(),
        1,
        "the send reached the lesche"
    );
}

/// A refresh that fails leaves the roster unknown, and an unknown roster
/// refuses nobody: the send goes through and the lesche gates the envelope
/// route itself. The entry below is built by the test seam -- a production
/// reader meets this branch as a stale slice, the case next door.
#[tokio::test]
async fn a_failed_refresh_judges_no_mention() {
    use std::sync::atomic::Ordering;

    let (state, id, mock) = setup_lesche_send(Vec::new()).await;
    mock.roster_status.store(500, Ordering::SeqCst);
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    assert!(
        send_with(
            state,
            id,
            Some("room-1"),
            None,
            vec![member_id("ada").as_ref().to_owned()],
        )
        .await
        .expect("an unconfirmed roster refuses nobody")
        .0
        .ok
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        1,
        "a rosterless cache asks the lesche once"
    );
    assert_eq!(
        mock.posted.lock().unwrap().len(),
        1,
        "the send reached the lesche"
    );
}

/// A stale slice and a failed refresh leave who is in the room
/// unconfirmed, so the send goes through after that one refresh,
/// never refused on a slice this call could not confirm.
#[tokio::test]
async fn a_stale_roster_and_a_failed_refresh_let_the_send_through() {
    use std::sync::atomic::Ordering;

    let ada = member_id("ada");
    let bo = member_id("bo");
    let (state, id, mock) =
        setup_lesche_send(vec![room_view_with("room-1", vec![ada.clone()])]).await;
    state
        .joined_rooms
        .set_room_snapshot("test", &[room_view_with("room-1", vec![ada])])
        .await;
    mock.roster_status.store(500, Ordering::SeqCst);

    assert!(
        send_with(
            state,
            id,
            Some("room-1"),
            None,
            vec![bo.as_ref().to_owned()],
        )
        .await
        .expect("an unconfirmed roster refuses nobody")
        .0
        .ok
    );
    assert_eq!(
        mock.pulls.load(Ordering::SeqCst),
        1,
        "a stale slice asks the lesche once"
    );
    assert_eq!(
        mock.posted.lock().unwrap().len(),
        1,
        "the send reached the lesche"
    );
}

/// A roomless send is judged for its target before its ids: the refusal
/// says the key rides a room send, not that an id is malformed.
#[tokio::test]
async fn a_roomless_send_hears_the_targeting_refusal_first() {
    let (state, id, _mock) = setup_lesche_send(Vec::new()).await;
    let err = send_with(
        state,
        id,
        None,
        Some("tagma-peer"),
        vec!["not-an-id".to_string()],
    )
    .await
    .expect_err("a direct-session send names no room");
    assert_eq!(err.status, 400);
    assert_eq!(
        err.message,
        "mentions ride a room send: a participant id names a member of that room"
    );
}

/// The field's three absences read the same: an omitted key, an explicit
/// null, and an empty list all mean this send names nobody.
#[test]
fn an_absent_null_or_empty_mention_list_names_nobody() {
    let parse =
        |body: &str| -> LescheMessageRequest { serde_json::from_str(body).expect("request json") };
    assert!(
        parse(r#"{"text":"ping","room":"room-1"}"#)
            .mentions
            .is_none()
    );
    assert!(
        parse(r#"{"text":"ping","room":"room-1","mentions":null}"#)
            .mentions
            .is_none(),
        "an explicit null names nobody"
    );
    assert_eq!(
        parse(r#"{"text":"ping","room":"room-1","mentions":[]}"#).mentions,
        Some(Vec::new())
    );
}

/// A stand-in lesche serving the room-history route: answers a fixed page
/// and keeps each request's query string, so a test can read what was asked.
type HistoryQueries = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

async fn spawn_room_history_stub() -> (String, HistoryQueries) {
    spawn_room_history_stub_serving(serde_json::json!([])).await
}

/// The same stub, serving a page of rows, so a test can drive the render
/// path with stored rows instead of an empty page.
async fn spawn_room_history_stub_serving(body: serde_json::Value) -> (String, HistoryQueries) {
    let seen: HistoryQueries = Default::default();
    let recorded = seen.clone();
    let app = axum::Router::new().route(
        "/v1/lesche/rooms/{_room}/messages",
        axum::routing::get(move |uri: axum::http::Uri| {
            let recorded = recorded.clone();
            let body = body.clone();
            async move {
                recorded
                    .lock()
                    .unwrap()
                    .push(uri.query().unwrap_or("").to_string());
                Json(body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1/lesche"), seen)
}

/// Read one stored room row through the route with this tagma as the
/// reader, and hand back the rendered block body. The row is a human peer's
/// (`Alice`), at a fixed position and timestamp, so a caller states only the
/// payload it wants the header to read.
async fn read_one_room_row(text: &str, quote: Option<i64>, mentions: Vec<MemberId>) -> String {
    use base64::Engine as _;
    let payload = serde_json::to_vec(&RoomMessage {
        text: text.into(),
        attachment: None,
        quote,
        mentions,
        thread: None,
    })
    .expect("room message json");
    let page = serde_json::json!([{
        "seq": 3,
        "sender": {"id": "p-user-1", "kind": "human", "handle": "Alice"},
        "epoch": 1,
        "ciphertext": base64::engine::general_purpose::STANDARD.encode(&payload),
        "created_at": "2026-08-02T00:00:00Z",
    }]);
    let (lesche_url, _asked) = spawn_room_history_stub_serving(page).await;
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );
    let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
        .build()
        .expect("lesche client");
    let handle = crate::relay::RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("t1".to_string()),
        "Tagma".into(),
        kallipai_e2ee::DeviceKey::generate(),
        id.clone(),
        std::sync::Arc::downgrade(&state),
    );
    crate::test_helpers::install_relay(&state, "test", handle);
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    read_room_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "room-1".to_string())),
        Query(LescheHistoryQuery {
            after_seq: None,
            before_seq: None,
            thread: None,
            limit: None,
        }),
    )
    .await
    .expect("a stored row renders")
}

/// Both cursors name a direction, so a request carrying both is rejected at
/// this hop in the lesche's own words -- not forwarded and surfaced as an
/// upstream 502, which reads as a retryable fault rather than a caller bug.
#[tokio::test]
async fn read_room_rejects_both_cursors_at_this_hop() {
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );

    let err = read_room_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "room-1".to_string())),
        Query(LescheHistoryQuery {
            after_seq: Some(2),
            before_seq: Some(8),
            thread: None,
            limit: None,
        }),
    )
    .await
    .expect_err("both cursors -> 400");
    assert_eq!(err.status, 400, "expected 400, got {}", err.status);
    assert_eq!(
        err.message,
        "after_seq and before_seq are mutually exclusive"
    );
}

/// The answer both room seams give a dot-segment id: the shape
/// sentence a caller reads, kept in one place so the pins cannot
/// drift.
const DOT_SEGMENT_ANSWER: &str = "room id: an id cannot be a dot segment";

/// A dot segment survives percent-encoding, so the room read
/// refuses it at this hop: left to the client, the value would
/// answer this hop's 503 (no relay owns it) rather than name the
/// request's own shape.
#[tokio::test]
async fn read_room_rejects_a_dot_segment_at_this_hop() {
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );

    let err = read_room_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "..".to_string())),
        Query(LescheHistoryQuery {
            after_seq: None,
            before_seq: None,
            thread: None,
            limit: None,
        }),
    )
    .await
    .expect_err("a dot segment -> 400");
    assert_eq!(err.status, 400, "expected 400, got {}", err.status);
    assert_eq!(err.message, DOT_SEGMENT_ANSWER);
}

/// The room send judges the same value the read does, before any relay is
/// resolved: a dot segment is this request's shape, not a backup path.
#[tokio::test]
async fn send_room_rejects_a_dot_segment_at_this_hop() {
    let state = make_state();

    let err = send_room_message(
        &state,
        "..".to_string(),
        "hello".into(),
        None,
        Vec::new(),
        None,
    )
    .await
    .expect_err("a dot segment -> 400");
    assert_eq!(err.status, 400, "expected 400, got {}", err.status);
    assert_eq!(err.message, DOT_SEGMENT_ANSWER);
}

/// The backward cursor crosses this hop: the lesche is asked for the page
/// below it, one row deep, and for nothing else.
#[tokio::test]
async fn read_room_forwards_the_backward_cursor() {
    let (lesche_url, asked) = spawn_room_history_stub().await;
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );
    let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
        .build()
        .expect("lesche client");
    let handle = crate::relay::RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("t1".to_string()),
        "Tagma".into(),
        kallipai_e2ee::DeviceKey::generate(),
        id.clone(),
        std::sync::Arc::downgrade(&state),
    );
    crate::test_helpers::install_relay(&state, "test", handle);
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    let body = read_room_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "room-1".to_string())),
        Query(LescheHistoryQuery {
            after_seq: None,
            before_seq: Some(8),
            thread: None,
            limit: Some(1),
        }),
    )
    .await
    .expect("an empty page renders as nothing");
    assert!(body.is_empty(), "empty page: {body}");

    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec!["before_seq=8&limit=1".to_string()],
        "one backward request, and no forward cursor on it"
    );
}

/// A discussion page crosses this hop with its root intact: the lesche is
/// asked for the thread position and a forward cursor, and nothing else is
/// added to the request.
#[tokio::test]
async fn read_room_forwards_the_thread_position() {
    let (lesche_url, asked) = spawn_room_history_stub().await;
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );
    let client = kallipai_lesche_client::LescheClient::builder(&lesche_url, "tok")
        .build()
        .expect("lesche client");
    let handle = crate::relay::RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("t1".to_string()),
        "Tagma".into(),
        kallipai_e2ee::DeviceKey::generate(),
        id.clone(),
        std::sync::Arc::downgrade(&state),
    );
    crate::test_helpers::install_relay(&state, "test", handle);
    state
        .joined_rooms
        .mark_rooms_rosterless("test", [RoomId::from("room-1".to_string())])
        .await;

    let body = read_room_messages(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path((id, "room-1".to_string())),
        Query(LescheHistoryQuery {
            after_seq: Some(0),
            before_seq: None,
            thread: Some(3),
            limit: Some(5),
        }),
    )
    .await
    .expect("an empty page renders as nothing");
    assert!(body.is_empty(), "empty page: {body}");

    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec!["after_seq=0&thread=3&limit=5".to_string()],
        "one discussion page, rooted and forward"
    );
}

/// The quoted seq and the reader's own mention cross the whole read path:
/// both sit inside the row's payload, and the rendered block puts them last
/// in the header, the mention behind the quoted position, immediately before
/// the closing bracket. The reader is the owning relay handle's tagma id,
/// so a row naming it reads `you`.
#[tokio::test]
async fn read_room_renders_the_quoted_seq_and_the_readers_mention() {
    let body = read_one_room_row(
        "answering that",
        Some(7),
        vec![MemberId::for_tagma(&TagmaId::from("t1".to_string()))],
    )
    .await;

    // The rendered timestamp of the row the helper stores, spelled as the
    // block spells it.
    assert_eq!(
        body,
        "[seq=3 from=human:p-user-1 \"Alice\" at=2026-08-02 0:00:00.0 +00:00:00 | quote=7 | mentions=you]\nanswering that\n\n"
    );
}

/// A row that names other members and not this tagma reads as their count:
/// the same viewer the live path takes, so the block states the reader's own
/// view rather than anything the room's roster holds.
#[tokio::test]
async fn read_room_counts_the_other_members_a_stored_row_names() {
    let body = read_one_room_row(
        "for ada",
        None,
        vec![MemberId::for_tagma(&TagmaId::from("ada".to_string()))],
    )
    .await;

    assert_eq!(
        body,
        "[seq=3 from=human:p-user-1 \"Alice\" at=2026-08-02 0:00:00.0 +00:00:00 | mentions=1]\nfor ada\n\n"
    );
}

/// A thread names a discussion inside a ROOM, so a send with no room -- the
/// bilateral 1:1 and a peer tagma's direct session -- answers one with 400.
/// It is a declaration, so it is refused on its own account: no quote rides it.
#[tokio::test]
async fn thread_without_a_room_is_bad_request() {
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );

    for (room, tagma) in [(None, None), (None, Some("peer".to_string()))] {
        let err = post_message(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id.clone()),
            Json(LescheMessageRequest {
                text: "hi".into(),
                room,
                tagma,
                attachment: None,
                quote: None,
                mentions: None,
                thread: Some(3),
            }),
        )
        .await
        .expect_err("a thread needs a room");
        assert_eq!(err.status, 400, "expected 400, got {}", err.status);
    }
}

/// A quote names a position in a conversation's own log, so the bilateral
/// 1:1 -- which has none -- answers a send carrying one with 400.
#[tokio::test]
async fn quote_on_a_bilateral_send_is_bad_request() {
    let state = make_state();
    let id = AgentId::random();
    state.registry.write().await.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, "tok".into())),
    );

    let err = post_message(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id.clone()),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: None,
            attachment: None,
            quote: Some(3),
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("a quote needs a target with a log");
    assert_eq!(err.status, 400, "expected 400, got {}", err.status);

    // The same send with a non-positive quote reads as no quote at all and
    // falls through to the bilateral path (unavailable in this harness).
    let err = post_message(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
        Path(id),
        Json(LescheMessageRequest {
            text: "hi".into(),
            room: None,
            tagma: None,
            attachment: None,
            quote: Some(0),
            mentions: None,
            thread: None,
        }),
    )
    .await
    .expect_err("no projector in this harness");
    assert_eq!(err.status, 503, "expected 503, got {}", err.status);
}
