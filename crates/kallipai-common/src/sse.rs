//! Server-side SSE helpers shared by `kallipai-tagma` and `kallipai-archeion`.
//!
//! [`OnDrop`] runs a synchronous closure exactly once when the SSE response
//! stream is dropped, so a per-connection resource (a presence entry, an
//! app-stream channel, a subscriber-transition log) can be released when the
//! HTTP client disconnects and axum/hyper drops the response body.
//!
//! Running the closure inline in `Drop::drop` (rather than spawning an async
//! future) is load-bearing: the closure observes the stream's inner fields
//! before they drop. For a `BroadcastStream`, that means a still-counted
//! receiver, so `receiver_count() == 1` reliably means "last subscriber" (see
//! the callers in `kallipai-tagma/src/sse.rs` and `kallipai-archeion/src/routes`).
//! The closures passed here must do only fast, synchronous work (e.g. acquire a
//! `std::sync` lock + mutate a map); blocking the dropping thread is
//! negligible.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::response::sse::Event;
use futures_core::Stream;

/// A stream wrapper that runs a closure exactly once when the stream is
/// dropped.
///
/// The inner stream and the closure are both boxed (`Pin<Box<...>>` /
/// `Box<dyn ...>`), so the struct is `Unpin` regardless of the inner stream's
/// `Unpin`-ness.
pub struct OnDrop {
    inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl OnDrop {
    /// Wrap `inner`, scheduling `on_drop` to run synchronously when this is
    /// dropped.
    pub fn new(
        inner: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
        on_drop: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            on_drop: Some(Box::new(on_drop)),
        }
    }
}

impl Stream for OnDrop {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().inner.as_mut().poll_next(cx)
    }
}

impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

/// Open a snapshot-then-live subscription: run `subscribe` first, then take
/// `snapshot`, returning the subscription and the initial events.
///
/// The body's ordering is the invariant. Because the subscription exists
/// before the snapshot is read, every change published after the snapshot
/// necessarily lands in the live subscription — an initial flush built from
/// the snapshot can only repeat an event, never miss one, and an idempotent
/// latest-wins consumer absorbs the repeat. The reverse order loses exactly
/// the events published in the capture-to-subscribe window: the snapshot is
/// stale and the live stream gaps until the next snapshot cadence.
///
/// Flushing `initial` is deliberately left to the caller, because the two SSE
/// surfaces flush differently: a channel-backed stream re-sends the events
/// into the subscribed channel (they interleave in channel order), while a
/// prepended stream emits them as the leading frames ahead of the merged
/// live sources.
pub async fn open_snapshot_stream<S, E, F, G, Fut>(subscribe: F, snapshot: G) -> (S, Vec<E>)
where
    F: FnOnce() -> S,
    G: FnOnce() -> Fut,
    Fut: Future<Output = Vec<E>>,
{
    let subscription = subscribe();
    let initial = snapshot().await;
    (subscription, initial)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The primitive's whole contract: the subscribe closure runs before the
    /// snapshot closure, and both results come back intact. A regression that
    /// swaps the two steps turns the ordering assert red.
    #[tokio::test]
    async fn subscribes_before_it_snapshots() {
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let subscribe_order = order.clone();
        let snapshot_order = order.clone();

        let (subscription, initial) = open_snapshot_stream(
            move || {
                subscribe_order.lock().unwrap().push("subscribe");
                "subscription"
            },
            move || {
                snapshot_order.lock().unwrap().push("snapshot");
                std::future::ready(vec![7u32, 9u32])
            },
        )
        .await;

        assert_eq!(subscription, "subscription");
        assert_eq!(initial, vec![7, 9]);
        assert_eq!(*order.lock().unwrap(), vec!["subscribe", "snapshot"]);
    }
}
