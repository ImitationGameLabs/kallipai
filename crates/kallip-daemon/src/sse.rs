use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use kallip_common::protocol::SseEvent;
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::warn;

/// A shutdown-aware SSE stream over one agent's broadcast channel.
///
/// The stream ends for two reasons:
/// - the broadcast sender is dropped (all subscribers gone / agent removed), or
/// - the daemon-wide `shutdown` token fires.
///
/// The shutdown arm is load-bearing: without it, a long-lived SSE connection
/// (e.g. an attached TUI) keeps the inner `BroadcastStream` open, so hyper's
/// `serve_connection` never completes and `axum::serve(...).with_graceful_shutdown`
/// never returns — Ctrl-C hangs. `take_until(shutdown.cancelled())` ends the
/// stream the instant the daemon-wide token fires, letting graceful shutdown
/// proceed.
///
/// The token passed here is the **daemon-wide** parent, not a per-agent child:
/// SSE shutdown is a daemon lifecycle concern, not an agent lifecycle one.
/// (Compare `bridge_task`, which uses the same parent token for its forced-exit
/// arm for the same reason.)
pub fn sse_stream(
    rx: tokio::sync::broadcast::Receiver<SseEvent>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(event_stream(rx, shutdown)).keep_alive(KeepAlive::default())
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
    use tokio::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Regression: without the `take_until(shutdown.cancelled())` arm, a
    /// long-lived SSE connection (e.g. an attached TUI) keeps the inner stream
    /// open and hangs graceful shutdown. The stream must end promptly when the
    /// daemon-wide shutdown token fires — proven here with a tight 100ms bound
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
}
