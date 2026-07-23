//! HTTP client for the kallip-lesche data-plane relay. See [`LescheClient`] for
//! the surface.

mod client;

pub use client::{LescheClient, LescheClientBuilder};
