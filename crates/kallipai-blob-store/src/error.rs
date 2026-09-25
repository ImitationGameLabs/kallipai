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

    /// Canonical packing caps the packed byte total (semantic anti-
    /// footgun bound, not an availability edge); the count is what the
    /// pack reached when it aborted.
    #[error("dossier packs to {size} bytes, over the {max}-byte cap")]
    DossierTooLarge { size: usize, max: usize },
    /// A canonical archive holds regular files and directories only.
    #[error("dossier contains a non-regular entry: {0}")]
    NonRegularEntry(String),
    /// A symlink could pull out-of-dossier content in or loop the walk.
    #[error("dossier contains a symlink entry: {0}")]
    SymlinkEntry(String),
    /// A non-UTF-8 name would be mangled into the tar entry name.
    #[error("dossier contains a non-UTF-8 entry name: {0}")]
    NonUtf8Entry(String),

    /// The blocking encode pass died (join error on the blocking pool).
    #[error("blob encode task failed: {0}")]
    Encode(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
