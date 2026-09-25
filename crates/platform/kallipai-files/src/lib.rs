//! The kallipai-files service: HTTP transfer over content-addressed
//! storage.
//!
//! The storage primitives (ids, write-once ingest, the [`BlobStore`]
//! seam, the local backend) live in the `kallipai-blob-store` crate; this crate
//! re-exports them for its API surface. The metadata layer (`metadata`,
//! `migration`) tracks uploads with reference counts and `gc` reclaims
//! unreferenced blobs.

pub mod gc;
pub mod metadata;
pub mod migration;
pub mod notify;

#[cfg(test)]
mod test_helpers;

pub use kallipai_blob_store::{
    BlobId, BlobInfo, BlobStore, Compression, Error, IngestPolicy, LocalBackend, PutOutcome,
};

pub mod acl;
pub mod api;
pub mod auth;
pub mod state;
