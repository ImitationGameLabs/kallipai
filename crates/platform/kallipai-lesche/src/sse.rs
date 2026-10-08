//! Relay SSE helpers.
//!
//! [`BoxEventStream`] is the boxed event-stream type used by the SSE faces
//! (app events, agent events). The synchronous-drop cleanup wrapper
//! ([`OnDrop`]) is shared from `kallipai_common::sse`.

use std::pin::Pin;

use axum::response::sse::Event;
use futures_util::Stream;

pub use kallipai_common::sse::OnDrop;

/// Boxed, erased SSE event stream shared by the app and agent SSE faces.
pub type BoxEventStream =
    Pin<Box<dyn Stream<Item = Result<Event, std::convert::Infallible>> + Send>>;
