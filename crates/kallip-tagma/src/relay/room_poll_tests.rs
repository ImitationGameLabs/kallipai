//! Tests for the slim room-membership poll pump: a mock lesche serving
//! `GET /tagmata/{tagma}/rooms`, driven by a real `AppState`. Proves the
//! pump refreshes the joined-rooms cache and tolerates a poll failure.

use super::*;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, extract::State, routing::get};
use kallip_lesche_common::direct::{DirectSessionId, DirectSessionPeer, DirectSessionView};
use kallip_lesche_common::rooms::{TagmaRoomView, Visibility};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::test_helpers::make_state;

/// A thread through which a test can mutate the rooms the mock serves on the
/// next poll.
type Rooms = Arc<Mutex<Vec<TagmaRoomView>>>;

async fn spawn_lesche(rooms: Rooms) -> String {
    let app = Router::new()
        .route(
            "/tagmata/{_tagma}/rooms",
            get(|State(rooms): State<Rooms>| async move { Json(rooms.lock().await.clone()) }),
        )
        .with_state(rooms);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn room_view(id: &str) -> TagmaRoomView {
    TagmaRoomView {
        room_id: RoomId::from(id.to_string()),
        members: Vec::new(),
        membership_epoch: 0,
        is_creator: false,
        visibility: Visibility::Private,
        name: None,
    }
}

async fn setup(rooms: Rooms) -> (RelayHandle, SharedState) {
    let state = make_state();
    let lesche_url = spawn_lesche(rooms).await;
    let client = LescheClient::builder(&lesche_url, "tok").build().unwrap();
    let handle = RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("tagma".to_string()),
        "Tagma".into(),
        DeviceKey::generate(),
        AgentId::from("root".to_string()),
        Arc::downgrade(&state),
    );
    (handle, state)
}

/// `poll_rooms` replaces the joined-rooms cache with the mock's snapshot: a
/// fresh tagma starts cold, one poll warms it, and a membership change is
/// reflected on the next poll (full replace).
#[tokio::test]
async fn poll_refreshes_joined_rooms_cache() {
    let rooms: Rooms = Arc::new(Mutex::new(vec![room_view("room-a")]));
    let (handle, state) = setup(rooms.clone()).await;

    assert!(
        !state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await,
        "cache starts empty before the first poll"
    );

    handle.poll_rooms().await;
    assert!(
        state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await,
        "first poll warms the cache"
    );
    assert!(
        !state
            .joined_rooms
            .is_joined(&RoomId::from("room-b".to_string()))
            .await,
        "a room outside the snapshot stays unknown"
    );

    rooms.lock().await.push(room_view("room-b"));
    handle.poll_rooms().await;
    assert!(
        state
            .joined_rooms
            .is_joined(&RoomId::from("room-b".to_string()))
            .await,
        "an added membership is picked up on the next poll"
    );

    rooms
        .lock()
        .await
        .retain(|v| v.room_id != RoomId::from("room-a".to_string()));
    handle.poll_rooms().await;
    assert!(
        !state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await,
        "a removed membership self-heals via the full replace"
    );
}

/// A poll against an unreachable lesche logs and leaves the previous cache
/// intact (best-effort; the next tick retries).
#[tokio::test]
async fn poll_failure_keeps_prior_cache() {
    let state = make_state();
    // No server behind this URL: the first poll fails.
    let client = LescheClient::builder("http://127.0.0.1:1", "tok")
        .build()
        .unwrap();
    let handle = RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("tagma".to_string()),
        "Tagma".into(),
        DeviceKey::generate(),
        AgentId::from("root".to_string()),
        Arc::downgrade(&state),
    );

    handle.poll_rooms().await;
    assert!(
        state.joined_rooms.joined_rooms().await.is_empty(),
        "a failed poll must not panic or corrupt the (empty) cache"
    );
}

/// A thread through which a test can flip the direct-sessions route to a
/// 500 (best-effort degrade probing).
type FailDirect = Arc<AtomicBool>;

/// The direct-session snapshot the mock serves on the next poll.
type Sessions = Arc<Mutex<Vec<DirectSessionView>>>;

/// The mock's shared state tuple (rooms, sessions, direct-fail flag).
type MockState = (Rooms, Sessions, FailDirect);

/// The full mock: rooms + direct-sessions routes, the latter flippable to
/// a 500 to exercise the sweep's best-effort degrade.
async fn spawn_full_lesche(rooms: Rooms, sessions: Sessions, fail: FailDirect) -> String {
    let app = Router::new()
        .route(
            "/tagmata/{_tagma}/rooms",
            get(|State(state): State<MockState>| async move { Json(state.0.lock().await.clone()) }),
        )
        .route(
            "/direct-sessions",
            get(|State(state): State<MockState>| async move {
                if state.2.load(Ordering::SeqCst) {
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                } else {
                    Json(state.1.lock().await.clone()).into_response()
                }
            }),
        )
        .with_state((rooms, sessions, fail));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn session_view(id: &str) -> DirectSessionView {
    DirectSessionView {
        session_id: DirectSessionId::from(id.to_string()),
        peer: DirectSessionPeer {
            tagma_id: TagmaId::from("peer-tagma".to_string()),
            handle: "peer-tagma@alice".to_string(),
        },
        created_at: OffsetDateTime::now_utc(),
    }
}

async fn setup_full(
    rooms: Rooms,
    sessions: Sessions,
    fail_direct: FailDirect,
) -> (RelayHandle, SharedState) {
    let state = make_state();
    let lesche_url = spawn_full_lesche(rooms, sessions, fail_direct).await;
    let client = LescheClient::builder(&lesche_url, "tok").build().unwrap();
    let handle = RelayHandle::new(
        client,
        "test".to_string(),
        TagmaId::from("tagma".to_string()),
        "Tagma".into(),
        DeviceKey::generate(),
        AgentId::from("root".to_string()),
        Arc::downgrade(&state),
    );
    (handle, state)
}

/// One sweep tick warms BOTH routing caches (rooms + direct sessions), and
/// each is a full replace of its relay slice: a removed membership
/// self-heals on the next sweep.
#[tokio::test]
async fn sweep_warms_rooms_and_direct_caches_with_full_replace() {
    let rooms: Rooms = Arc::new(Mutex::new(vec![room_view("room-a")]));
    let sessions: Sessions = Arc::new(Mutex::new(vec![session_view("sess-a")]));
    let (handle, state) = setup_full(rooms.clone(), sessions.clone(), Arc::default()).await;

    handle.poll_sweep().await;
    assert!(
        state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await,
        "the sweep warms the rooms cache"
    );
    assert!(
        state
            .direct_sessions
            .is_session(&DirectSessionId::from("sess-a".to_string()))
            .await,
        "the sweep warms the direct-session cache"
    );

    sessions.lock().await.clear();
    handle.poll_sweep().await;
    assert!(
        !state
            .direct_sessions
            .is_session(&DirectSessionId::from("sess-a".to_string()))
            .await,
        "a removed session self-heals via the full replace"
    );
}

/// A failing direct-sessions list must not take the rooms refresh down
/// with it (each sweep call is best-effort): the rooms cache still warms,
/// the direct cache stays cold, nothing panics.
#[tokio::test]
async fn direct_poll_failure_keeps_the_rooms_cache_warm() {
    let rooms: Rooms = Arc::new(Mutex::new(vec![room_view("room-a")]));
    let sessions: Sessions = Arc::new(Mutex::new(vec![session_view("sess-a")]));
    let fail: FailDirect = Arc::new(AtomicBool::new(true));
    let (handle, state) = setup_full(rooms, sessions, fail).await;

    handle.poll_sweep().await;
    assert!(
        state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await,
        "the rooms refresh survives the direct poll failure"
    );
    assert!(
        !state
            .direct_sessions
            .is_session(&DirectSessionId::from("sess-a".to_string()))
            .await,
        "the direct cache stays cold when its poll fails"
    );
}

/// The room pump's slot lifecycle: start runs the immediate first tick
/// (warming the joined-rooms cache), stop clears the slot, and a restart
/// installs a fresh pump.
#[tokio::test]
async fn room_pump_slot_start_stop_restart() {
    let rooms: Rooms = Arc::new(Mutex::new(vec![room_view("room-a")]));
    let (handle, state) = setup(rooms).await;

    assert!(
        !handle.inner.room_pump.is_running().await,
        "the slot starts empty"
    );
    handle.start_room_pump().await;
    assert!(
        handle.inner.room_pump.is_running().await,
        "start must install the pump"
    );

    // The first tick is immediate: wait out the spawned poll briefly.
    for _ in 0..100 {
        if state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        state
            .joined_rooms
            .is_joined(&RoomId::from("room-a".to_string()))
            .await,
        "the immediate first tick warms the cache"
    );

    handle.stop_room_pump().await;
    assert!(
        !handle.inner.room_pump.is_running().await,
        "stop must clear the slot"
    );

    handle.start_room_pump().await;
    assert!(
        handle.inner.room_pump.is_running().await,
        "a stopped slot must accept a fresh pump"
    );
    handle.stop_room_pump().await;
}
