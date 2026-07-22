//! The multiplexed app event stream (`GET /v1/me/events`). One connection per
//! user carries envelope deliveries for all of their conversations plus presence
//! transitions (`TagmaOnline`/`TagmaOffline`). On open, the stream emits the
//! current presence snapshot for the user's online tagmata; changes then arrive
//! incrementally as herald tunnels connect/disconnect.
//!
//! If a slow client falls behind the broadcast capacity, the channel drops
//! events server-side (logged at `warn`); the client must reconnect/resync.

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use kallip_agora_common::event::AgoraEvent;
use kallip_common::protocol::ApiError;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_user};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::SharedConvState;

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/me/events", get(me_events))
}

async fn me_events(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Sse<OnDrop>, ApiError> {
    let user_id = require_user(&principal)?.clone();
    let tx = {
        let mut reg = state.write()?;
        reg.open_app_stream(&user_id)
    };
    let rx = tx.subscribe();
    // Presence snapshot: emit TagmaOnline for each of the user's currently-online
    // tagmata. Read presence after the receiver is subscribed so the sends land.
    // A tunnel connecting concurrently may be delivered twice (once here, once
    // as its own live TagmaOnline); clients MUST treat presence as an idempotent
    // set, not assume exactly-once. No online tagma is missed.
    {
        let reg = state.read()?;
        for (tagma_id, entry) in reg.presence.iter() {
            if entry.owner == user_id {
                let _ = tx.send(AgoraEvent::TagmaOnline {
                    tagma_id: tagma_id.clone(),
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
    let cleaned = OnDrop::new(stream, move || {
        let Ok(mut reg) = cleanup_state.write() else {
            return;
        };
        reg.remove_app_stream_if_last(&cleanup_user, &cleanup_tx);
    });
    Ok(Sse::new(cleaned))
}
