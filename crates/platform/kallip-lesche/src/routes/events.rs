//! The multiplexed app event stream (`GET /v1/me/events`). One connection per
//! user carries envelope deliveries for all of their conversations plus presence
//! transitions: `TagmaOnline`/`TagmaOffline` for the user's own tagmata, and
//! `RoomMemberOnline`/`RoomMemberOffline` for peers in the user's rooms (fanned
//! by [`crate::room_presence`] as participants connect/disconnect). On open, the
//! stream emits the current presence snapshot for the user's online tagmata;
//! room-member presence arrives live and is resynced by the roster's `online`
//! field on each roster fetch.
//!
//! If a slow client falls behind the broadcast capacity, the channel drops
//! events server-side (logged at `warn`); the client must reconnect/resync.

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::event::LescheEvent;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_user};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::SharedConvState;
use kallip_agora_common::ids::ParticipantId;

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/me/events", get(me_events))
}

async fn me_events(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Sse<OnDrop>, ApiError> {
    let user_id = require_user(&principal)?.clone();
    // Capture the runtime handle so the synchronous `OnDrop` cleanup can spawn
    // the presence fan-out without `tokio::spawn`'s implicit `Handle::current()`
    // panic (Drop may run off-runtime during body teardown).
    let handle = tokio::runtime::Handle::try_current().ok();
    let (tx, was_first) = {
        let mut reg = state.write()?;
        // The 0 -> 1 edge: announce room presence only on the user's FIRST live
        // app stream, so a second tab re-opening does not re-announce. Race-free
        // under this write lock; relies on `me_events` being the sole
        // `app_streams` creator (state.rs invariant #3) -- a future caller
        // outside `me_events` would need to revisit.
        let was_first = !reg.has_app_stream(&user_id);
        let tx = reg.open_app_stream(&user_id);
        (tx, was_first)
    };
    // Announce room-member presence to peers (best-effort, off the request path),
    // spawned AFTER the write guard releases -- matching the tunnel connect path
    // and keeping every fan-out site uniform: no spawn under a registry lock.
    if was_first && let Some(h) = &handle {
        let st = state.clone();
        let who = ParticipantId::for_user(&user_id);
        h.spawn(async move {
            crate::room_presence::fan_member_presence(&st, &who, true).await;
        });
    }
    let rx = tx.subscribe();
    // Presence snapshot: emit TagmaOnline for each of the user's currently-online
    // tagmata. Read presence after the receiver is subscribed so the sends land.
    // A tunnel connecting concurrently may be delivered twice (once here, once
    // as its own live TagmaOnline); clients MUST treat presence as an idempotent
    // set, not assume exactly-once. No online tagma is missed.
    {
        let reg = state.read()?;
        for entry in reg.presence.values() {
            if entry.owner == user_id {
                let _ = tx.send(LescheEvent::TagmaOnline {
                    tagma_id: entry.tagma_id.clone(),
                });
            }
        }
    }
    let stream: BoxEventStream = Box::pin(
        BroadcastStream::new(rx)
            .filter_map(|r| match r {
                Ok(ev) => Some(ev),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!(lag = n, "app SSE subscriber lagged; events dropped");
                    None
                }
            })
            .map(|ev| {
                Ok::<Event, std::convert::Infallible>(
                    Event::default().json_data(ev).expect("event serializes"),
                )
            }),
    );

    // tx is the Sender cloned from the map; `receiver_count()` includes our own
    // subscribed rx (still alive during this closure), so `== 1` == "last one".
    let cleanup_state = state.clone();
    let cleanup_user = user_id.clone();
    let cleanup_tx = tx.clone();
    let cleanup_handle = handle.clone();
    let cleaned = OnDrop::new(stream, move || {
        let removed = {
            let Ok(mut reg) = cleanup_state.write() else {
                return;
            };
            reg.remove_app_stream_if_last(&cleanup_user, &cleanup_tx)
        };
        // On the 1 -> 0 edge, fan room-member offline to peers. The guard is
        // dropped above; the (async) fan-out is spawned off-thread.
        if removed {
            if let Some(h) = cleanup_handle.as_ref() {
                let st = cleanup_state.clone();
                let who = ParticipantId::for_user(&cleanup_user);
                h.spawn(async move {
                    crate::room_presence::fan_member_presence(&st, &who, false).await;
                });
            } else {
                tracing::warn!("offline-presence fan skipped: no runtime at drop");
            }
        }
    });
    Ok(Sse::new(cleaned))
}
