//! Tagma-side HTTP client spanning both relay families. See [`LescheClient`]
//! for the surface.

mod client;

pub use client::{
    LescheClient, LescheClientBuilder, LescheHttpError, RoomHistoryQuery, UpstreamAck,
    UpstreamFaceCounts,
};
