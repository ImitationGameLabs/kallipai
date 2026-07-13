//! Agora SSE helpers.
//!
//! [`BoxEventStream`] is the boxed event-stream type used by the herald tunnel
//! and the app SSE. The synchronous-drop cleanup wrapper ([`OnDrop`]) is shared
//! from `kallip_common::sse`.

use std::pin::Pin;

use axum::response::sse::Event;
use futures_util::Stream;

pub use kallip_common::sse::OnDrop;

/// Boxed, erased SSE event stream shared by the herald tunnel and the app SSE.
pub type BoxEventStream =
    Pin<Box<dyn Stream<Item = Result<Event, std::convert::Infallible>> + Send>>;
