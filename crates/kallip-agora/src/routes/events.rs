//! The multiplexed app event stream (`GET /v1/me/events`). One connection per
//! user carries envelope deliveries for all of their conversations. (Presence
//! change events are deferred; the app polls `/v1/tagmata` for now.)
//!
//! If a slow client falls behind the broadcast capacity, the channel drops
//! events server-side (logged at `warn`); the client must reconnect/resync.
//!
//! The `app_streams` entry is created by `me_events` via
//! [`Registry::open_app_stream`](crate::state::Registry::open_app_stream) (the
//! sole creator) and removed synchronously in `Drop::drop` when the last
//! subscriber disconnects. `OnDrop`'s closure runs before the inner
//! `BroadcastStream` (and its `rx`) is dropped, so `receiver_count() == 1`
//! reliably means "I am the last subscriber; after this drop none remain." The
//! removal runs under the registry write lock, which a concurrent new
//! subscriber must also take to insert/subscribe; if the newcomer arrives after
//! the removal it simply recreates the entry, so the only observable outcomes
//! are "removed" or "recreated," never a leaked or wrongly-shared channel.

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use kallip_common::protocol::ApiError;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_user};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new().route("/me/events", get(me_events))
}

async fn me_events(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Sse<OnDrop>, ApiError> {
    let user_id = require_user(&principal)?.clone();
    let tx = {
        let mut reg = state.write()?;
        reg.open_app_stream(&user_id)
    };
    let rx = tx.subscribe();
    let stream: BoxEventStream = Box::pin(
        BroadcastStream::new(rx)
            .filter_map(|r| match r {
                Ok(ev) => Some(ev),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    // The client fell behind the channel capacity: events were
                    // dropped server-side. Log it so the operator sees the loss;
                    // the client must reconnect/resync to recover.
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
