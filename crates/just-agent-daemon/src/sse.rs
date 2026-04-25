use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use just_agent_core::types::SseEvent;
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

pub fn sse_stream(
    rx: tokio::sync::broadcast::Receiver<SseEvent>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(event) => {
                let data = serde_json::to_string(&event).ok()?;
                Some(Ok(Event::default().data(data)))
            }
            Err(_) => None, // skip lagged messages
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
