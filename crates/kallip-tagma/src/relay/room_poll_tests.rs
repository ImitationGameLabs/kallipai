//! Tests for the slim room-membership poll pump: a mock lesche serving
//! `GET /v1/tagmata/{tagma}/rooms`, driven by a real `AppState`. Proves the
//! pump refreshes the joined-rooms cache and tolerates a poll failure.

use super::*;
use axum::{Json, Router, extract::State, routing::get};
use kallip_lesche_common::rooms::{TagmaRoomView, Visibility};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::test_helpers::make_state;

/// A thread through which a test can mutate the rooms the mock serves on the
/// next poll.
type Rooms = Arc<Mutex<Vec<TagmaRoomView>>>;

async fn spawn_lesche(rooms: Rooms) -> String {
    let app = Router::new()
        .route(
            "/v1/tagmata/{_tagma}/rooms",
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
