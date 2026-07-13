//! Server-side SSE helpers shared by `kallip-daemon` and `kallip-agora`.
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
//! the callers in `kallip-daemon/src/sse.rs` and `kallip-agora/src/routes`).
//! The closures passed here must do only fast, synchronous work (e.g. acquire a
//! `std::sync` lock + mutate a map); blocking the dropping thread is
//! negligible.

use std::convert::Infallible;
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
