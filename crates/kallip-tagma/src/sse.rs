use std::convert::Infallible;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::SseEvent;
use kallip_common::sse::OnDrop;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// A shutdown-aware SSE stream over one agent's broadcast channel, with
/// subscribe/unsubscribe transition logging.
///
/// The stream ends for two reasons:
/// - the broadcast sender is dropped (all subscribers gone / agent removed), or
/// - the tagma-wide `shutdown` token fires.
///
/// The shutdown arm is load-bearing: without it, a long-lived SSE connection
/// (e.g. an attached TUI) keeps the inner `BroadcastStream` open, so hyper's
/// `serve_connection` never completes and `axum::serve(...).with_graceful_shutdown`
/// never returns — Ctrl-C hangs. `take_until(shutdown.cancelled())` ends the
/// stream the instant the tagma-wide token fires, letting graceful shutdown
/// proceed.
///
/// The token passed here is the **tagma-wide** parent, not a per-agent child:
/// SSE shutdown is a tagma lifecycle concern, not an agent lifecycle one.
/// (Compare `bridge_task`, which uses the same parent token for its forced-exit
/// arm for the same reason.)
///
/// # Subscriber-state logging
///
/// Subscribe/unsubscribe is the source of truth for "are this agent's runtime
/// events being observed." We log exactly the `0 <-> 1` transitions of
/// `events_tx.receiver_count()` here — not per runtime event, which would spam
/// on every token delta for any agent (notably subagents) that runs without a
/// subscriber. Attach is logged synchronously in this function; detach is logged
/// from an [`OnDrop`] guard whose closure runs when the stream is dropped.
pub fn sse_stream(
    id: AgentId,
    events_tx: broadcast::Sender<SseEvent>,
    rx: broadcast::Receiver<SseEvent>,
    shutdown: CancellationToken,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // 0 -> 1 transition: this subscriber is the first (and, right now, only)
    // one. `subscribe()` already ran at the call site, so the count includes us.
    if events_tx.receiver_count() == 1 {
        info!(id = %id, "SSE subscriber attached");
    }

    // Detach fires from the `OnDrop` guard's `Drop::drop`, which runs before
    // any field is dropped — in particular before the inner broadcast receiver,
    // whose `receiver_count()` decrement is what `should_log_detach` keys off
    // of. Invariant relied on below: the closure observes the receiver still
    // counted. Re-verify if `event_stream` is ever wrapped in a combinator with
    // a custom `Drop` that could drop an inner field early.
    let detach_id = id.clone();
    let detach_tx = events_tx.clone();
    let detach_shutdown = shutdown.clone();

    Sse::new(OnDrop::new(event_stream(rx, shutdown), move || {
        if should_log_detach(&detach_shutdown, &detach_tx) {
            info!(id = %detach_id, "SSE subscriber detached");
        }
    }))
    .keep_alive(KeepAlive::default())
}

/// Whether dropping one subscriber right now should be logged as the
/// "no one is watching" transition.
///
/// - `receiver_count() == 1`: this connection's receiver is still alive in
///   `Drop::drop` (Rust drops fields after the `Drop::drop` body), so a count of
///   1 means we are the last — after the drop completes, none remain. With more
///   than one subscriber, dropping one leaves others watching, so no transition.
/// - `!shutdown.is_cancelled()`: on tagma-wide shutdown every still-attached
///   stream ends via `take_until`, which would otherwise emit a misleading
///   "detached" line per connection (nothing is left to reattach to).
///
/// Best-effort: `receiver_count()` is atomic but a concurrent subscribe/detach
/// can at worst yield one misleading `info`-level line.
fn should_log_detach(
    shutdown: &CancellationToken,
    events_tx: &broadcast::Sender<SseEvent>,
) -> bool {
    !shutdown.is_cancelled() && events_tx.receiver_count() == 1
}

/// Build the shutdown-aware event stream without the SSE/keepalive framing.
///
/// Extracted from [`sse_stream`] so the shutdown contract is directly
/// unit-testable (an `axum::response::Sse` is a response body, not a pollable
/// stream).
///
/// `take_until` lives in `futures_util::stream::StreamExt`; it is applied here
/// via its fully-qualified path so the surrounding sync `filter_map` (from
/// `tokio_stream::StreamExt`, whose closure returns `Option<T>` rather than a
/// future) stays unambiguous.
fn event_stream(
    rx: tokio::sync::broadcast::Receiver<SseEvent>,
    shutdown: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::StreamExt::take_until(
        BroadcastStream::new(rx),
        shutdown.cancelled_owned(),
    )
    .filter_map(|result| match result {
        Ok(event) => match serde_json::to_string(&event) {
            Ok(data) => Some(Ok(Event::default().data(data))),
            Err(e) => {
                warn!(error = %e, "failed to serialize SSE event, dropping");
                None
            }
        },
        Err(_) => None, // skip lagged messages
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::protocol::SseEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Regression: without the `take_until(shutdown.cancelled())` arm, a
    /// long-lived SSE connection (e.g. an attached TUI) keeps the inner stream
    /// open and hangs graceful shutdown. The stream must end promptly when the
    /// tagma-wide shutdown token fires — proven here with a tight 100ms bound
    /// (a regression that re-introduces a seconds-long park would be caught).
    #[tokio::test]
    async fn sse_stream_ends_on_shutdown() {
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<SseEvent>(16);
        let rx = events_tx.subscribe();
        let shutdown = CancellationToken::new();

        let stream = event_stream(rx, shutdown.clone());
        tokio::pin!(stream);

        // No events have been sent, so the stream is parked — only the
        // shutdown arm can end it.
        shutdown.cancel();

        let next = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        assert!(
            matches!(next, Ok(None)),
            "SSE stream did not end cleanly after shutdown fired"
        );
    }

    /// A pending event is still delivered when shutdown has not fired.
    ///
    /// Note: `take_until` polls the shutdown future *before* the inner stream,
    /// so a shutdown that is already cancelled by the time the stream is polled
    /// wins and drops the queued event. This test isolates the delivery path by
    /// leaving the token uncancelled.
    #[tokio::test]
    async fn sse_stream_delivers_queued_event() {
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<SseEvent>(16);
        let rx = events_tx.subscribe();
        let shutdown = CancellationToken::new();

        let stream = event_stream(rx, shutdown);
        tokio::pin!(stream);

        // Subscribe first, then publish — broadcast only reaches live receivers.
        events_tx
            .send(SseEvent::Status {
                message: "hi".into(),
            })
            .unwrap();

        // First poll yields the queued event before shutdown ends the stream.
        let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("timed out waiting for first event")
            .expect("stream ended before yielding the event");
        assert!(first.is_ok(), "queued event should have been delivered");
    }

    /// The `OnDrop` guard fires its closure exactly once when the wrapped
    /// stream is dropped — the mechanism that lets `sse_stream` detect a
    /// subscriber detaching.
    #[tokio::test]
    async fn on_drop_runs_closure_when_stream_dropped() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_for_closure = fired.clone();

        // Inner stream type doesn't matter here — the test never polls it, only
        // asserts the closure runs on drop. Use an empty stream with the Item
        // type `OnDrop` is pinned to.
        let stream = futures_util::stream::empty::<Result<Event, Infallible>>();
        let wrapped = OnDrop::new(stream, move || {
            fired_for_closure.store(true, Ordering::SeqCst);
        });
        drop(wrapped);

        assert!(
            fired.load(Ordering::SeqCst),
            "OnDrop closure must run when the wrapped stream is dropped"
        );
    }

    /// `should_log_detach` is the precise detach-transition predicate: only when
    /// this is the last subscriber AND the tagma is not shutting down.
    #[tokio::test]
    async fn should_log_detach_only_when_last_subscriber_and_not_shutting_down() {
        // Single subscriber: last one -> log.
        let (tx, _rx) = tokio::sync::broadcast::channel::<SseEvent>(4);
        assert!(should_log_detach(&CancellationToken::new(), &tx));

        // Two subscribers: dropping one leaves another -> no log.
        let _second = tx.subscribe();
        assert!(!should_log_detach(&CancellationToken::new(), &tx));

        // Tagma shutting down: suppress even if this is the last subscriber.
        let (tx2, _rx2) = tokio::sync::broadcast::channel::<SseEvent>(4);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert!(!should_log_detach(&shutdown, &tx2));
    }
}
