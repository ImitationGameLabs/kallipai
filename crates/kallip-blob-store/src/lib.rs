//! The content-addressed blob store: hash-addressed ids,
//! write-once ingest, and range reads over a sharded on-disk layout.
//!
//! Content is addressed by its SHA-256 digest: the id is the algorithm
//! prefix plus the full lowercase hex digest, never truncated, so safety
//! reduces to hash safety with no collision-handling code, and identical
//! uploads deduplicate by construction.
//!
//! [`BlobStore`] is the object-safe seam callers hold as
//! `Arc<dyn BlobStore>`: `put` streams a reader through hashing and to
//! disk in one pass, `get_range` serves byte windows, and further
//! backends can arrive behind the same trait. [`LocalBackend`] is the
//! local-disk implementation; the layout is
//! `<root>/blobs/<first two hex characters>/<id>` with staging under
//! `<root>/tmp/`, both on one volume so commits are atomic renames.

pub mod compression;
pub mod encode;
pub mod error;
pub mod hash;
mod ingest;
pub mod local;
pub mod rewrite;
pub mod store;

pub use self::compression::Compression;
pub use self::error::Error;
pub use self::hash::BlobId;
pub use self::local::LocalBackend;
pub use self::rewrite::{CorruptPolicy, RewriteOptions, RewriteReport, rewrite_root};
pub use self::store::{BlobInfo, BlobStore, IngestPolicy, PutOutcome};

/// The on-disk layout for one blob: `<root>/blobs/<bucket>/<id>`.
///
/// Exposed for callers that verify the layout directly -- the files
/// service's garbage collector re-checks the path before unlinking, so
/// it must be able to compute the same address the ingest committed to.
pub use ingest::blob_path;
