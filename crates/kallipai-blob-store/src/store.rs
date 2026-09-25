//! The blob store seam: one object-safe trait storage consumers program
//! against, independent of where bytes actually live.

use async_trait::async_trait;
use tokio::io::AsyncRead;

use crate::error::Error;
use crate::hash::BlobId;

/// Metadata for one stored blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    pub id: BlobId,
    pub size: u64,
}

/// The ingest-side representation policy: which representation new
/// blobs get and the logical-size ceiling above which an object stays
/// raw anyway (a compressed ranged read decodes the whole frame, so
/// very large objects keep raw cost and memory behavior). Reads are
/// self-identifying: they decode a frame whose content hashes back to
/// the address and pass anything else through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestPolicy {
    pub compression: crate::compression::Compression,
    pub compress_above: Option<u64>,
}

/// What a successful `put` produced: the content address of the
/// stored object (dedup means the address may already have existed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    pub id: BlobId,
}

/// The full blob surface, backend-agnostic.
///
/// The trait is object-safe so services can hold an
/// `Arc<dyn BlobStore>` and stay ignorant of the backend (local disk
/// now, a cloud object store later), the same seam shape as the
/// instances `InstanceBackend`. `put` takes a `dyn AsyncRead` -- a
/// generic parameter would opt the trait out of object safety -- so an
/// upload body streams through in one pass: hashing and bytes move
/// together, nothing is ever fully buffered.
///
/// Range contract, fixed for every backend: `(offset, len) -> bytes`.
/// An `offset` at or past the end of the content is
/// [`Error::RangeOutOfBounds`]; a `len` running past the end is
/// clamped to the available bytes, so open-ended reads from the
/// start to EOF must succeed.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Stream `content` in, store it, return the content address.
    /// Uploading the same bytes again is idempotent: the store keeps
    /// exactly one copy per digest.
    async fn put(&self, content: &mut (dyn AsyncRead + Unpin + Send)) -> Result<PutOutcome, Error>;

    /// Read the whole blob into memory, sized to the blob (the upload
    /// size cap bounds it); prefer [`Self::get_range`] windows when
    /// serving large blobs.
    async fn get(&self, id: &BlobId) -> Result<Vec<u8>, Error>;

    /// Read up to `len` bytes starting at `offset` (see the range
    /// contract above).
    async fn get_range(&self, id: &BlobId, offset: u64, len: u64) -> Result<Vec<u8>, Error>;

    /// Size of the blob, or `None` when absent. `None` rather than an
    /// error keeps existence probes out of the error path.
    async fn stat(&self, id: &BlobId) -> Result<Option<BlobInfo>, Error>;

    /// Remove the blob. Idempotent: deleting an absent id succeeds, so
    /// a garbage collector that re-checks before unlinking cannot trip
    /// over a concurrent delete in its check-to-unlink window.
    async fn delete(&self, id: &BlobId) -> Result<(), Error>;
}
