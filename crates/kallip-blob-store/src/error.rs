//! Error vocabulary for the blob store.

use crate::hash::BlobId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested blob does not exist. Missing and invalid ids stay
    /// distinct errors so consumers can tell absent content from malformed ids.
    #[error("blob not found: {0}")]
    NotFound(BlobId),

    /// A blob id failed validation: wrong algorithm prefix, wrong
    /// length, or non-canonical hex.
    #[error("invalid blob id: {0}")]
    InvalidId(String),

    /// A read range started at or past the end of the content. A `len`
    /// running past the end is clamped instead (open-ended ranges must
    /// succeed), so this fires only when there is nothing to serve.
    #[error("range out of bounds: offset {offset} beyond size {size}")]
    RangeOutOfBounds { offset: u64, len: u64, size: u64 },

    /// The OS entropy source failed; no temp file name could be minted.
    #[error("entropy source failure: {0}")]
    Rng(String),

    /// The blocking encode pass died (join error on the blocking pool).
    #[error("blob encode task failed: {0}")]
    Encode(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
