//! HTTP client for the kallipai-lesche data-plane relay. See [`LescheClient`] for
//! the surface.

mod client;

pub use client::{
    LescheClient, LescheClientBuilder, LescheHttpError, UpstreamAck, UpstreamFaceCounts,
};
