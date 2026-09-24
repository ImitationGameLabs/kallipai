//! The local-disk backend: blobs under `<root>/blobs/`, staging under
//! `<root>/tmp/`, both on one volume so commits are atomic renames.
//!
//! Durability: `put` fsyncs the staging file before the rename, so the
//! bytes are on disk when it returns; the directory entry itself is not
//! fsynced, so a power loss right after a commit can still lose the
//! rename. Content addressing makes a client retry a safe,
//! byte-identical heal.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::fs;
use tokio::io::AsyncRead;

use crate::ingest;
use crate::store::{BlobInfo, BlobStore, PutOutcome};
use crate::{BlobId, Error};

/// Content-addressed storage on the local filesystem.
pub struct LocalBackend {
    root: PathBuf,
    policy: crate::store::IngestPolicy,
}

impl LocalBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            policy: crate::store::IngestPolicy {
                compression: crate::compression::Compression::Off,
                compress_above: None,
            },
        }
    }

    /// A backend with the given on-disk representation for newly
    /// ingested blobs. Reads stay compatible either way: the zstd
    /// frame magic distinguishes representations on the way out.
    pub fn with_compression(
        root: impl Into<PathBuf>,
        compression: crate::compression::Compression,
    ) -> Self {
        Self {
            root: root.into(),
            policy: crate::store::IngestPolicy {
                compression,
                compress_above: None,
            },
        }
    }

    /// A backend with a full ingest policy: representation for new
    /// blobs plus the size threshold above which objects stay raw.
    pub fn with_policy(root: impl Into<PathBuf>, policy: crate::store::IngestPolicy) -> Self {
        Self {
            root: root.into(),
            policy,
        }
    }

    /// The backend behind the object-safe seam, ready for service state
    /// (services hold an `Arc<dyn BlobStore>`).
    pub fn arc(root: impl Into<PathBuf>) -> Arc<dyn BlobStore> {
        Arc::new(Self::new(root))
    }
    /// The object-safe seam with compression, mirroring [`Self::arc`].
    pub fn arc_with(
        root: impl Into<PathBuf>,
        compression: crate::compression::Compression,
    ) -> Arc<dyn BlobStore> {
        Arc::new(Self::with_compression(root, compression))
    }
    /// The object-safe seam with the full ingest policy.
    pub fn arc_with_policy(
        root: impl Into<PathBuf>,
        policy: crate::store::IngestPolicy,
    ) -> Arc<dyn BlobStore> {
        Arc::new(Self::with_policy(root, policy))
    }

    fn blob_path(&self, id: &BlobId) -> PathBuf {
        ingest::blob_path(&self.root, id)
    }
}

/// A missing file is the caller-facing `NotFound`/`None` everywhere;
/// anything else stays an IO error.
fn map_missing(err: io::Error, id: &BlobId) -> Error {
    if err.kind() == io::ErrorKind::NotFound {
        Error::NotFound(id.clone())
    } else {
        err.into()
    }
}

/// The logical size for a stored representation: a zstd frame's
/// header carries the pledged source size, anything else is stored
/// length. Falls back to the stored length when the header cannot be
/// parsed (the caller will surface the decode error on read).
fn logical_size(stored_head: &[u8], stored_len: u64) -> u64 {
    if crate::compression::is_zstd_frame(stored_head) {
        zstd_safe::get_frame_content_size(stored_head)
            .ok()
            .flatten()
            .unwrap_or(stored_len)
    } else {
        stored_len
    }
}

/// Logical bytes from stored bytes, with the address as the integrity
/// anchor: a zstd frame is decoded only when the decoded content
/// hashes back to the blob's address. Bytes that merely look like a
/// frame (a magic collision, a corrupt stream, a hash that does not
/// come home) pass through unchanged -- for them, raw IS the correct
/// content. Corrupt frames and corrupt raw get the same treatment
/// here; damage is surfaced by the rewrite tool's hash verification
/// and by upper layers, not by a second opinion in the read path.
/// Windowed reads pay this once per whole-frame decode, not per
/// window: the caller decodes once and slices.
fn logical_bytes(id: &BlobId, stored: Vec<u8>) -> Vec<u8> {
    if crate::compression::is_zstd_frame(&stored)
        && let Ok(decoded) = crate::compression::decode(&stored)
    {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&decoded);
        if hasher.finalize().as_slice() == id.digest() {
            return decoded;
        }
    }
    stored
}

#[async_trait]
impl BlobStore for LocalBackend {
    async fn put(&self, content: &mut (dyn AsyncRead + Unpin + Send)) -> Result<PutOutcome, Error> {
        ingest::ingest(
            &self.root,
            content,
            self.policy.compression,
            self.policy.compress_above,
        )
        .await
    }

    async fn get(&self, id: &BlobId) -> Result<Vec<u8>, Error> {
        let raw = fs::read(self.blob_path(id))
            .await
            .map_err(|err| map_missing(err, id))?;
        Ok(logical_bytes(id, raw))
    }

    async fn get_range(&self, id: &BlobId, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
        // The range contract is over the LOGICAL bytes. For a compressed
        // representation that means decompressing the whole frame first:
        // zstd has no random access, and ranged reads are only consumed
        // by the files service (kallip-files/src/state.rs, the
        // `LocalBackend::arc` assembly), which runs compression off.
        let raw = fs::read(self.blob_path(id))
            .await
            .map_err(|err| map_missing(err, id))?;
        let logical = logical_bytes(id, raw);
        if offset >= logical.len() as u64 {
            return Err(Error::RangeOutOfBounds {
                offset,
                len,
                size: logical.len() as u64,
            });
        }
        let start = offset as usize;
        let end = start.saturating_add(len as usize).min(logical.len());
        Ok(logical[start..end].to_vec())
    }

    async fn stat(&self, id: &BlobId) -> Result<Option<BlobInfo>, Error> {
        // The reported size is the LOGICAL size (the bytes `get`
        // returns), computed with the same self-identification the
        // read path uses: only a frame whose decoded content hashes
        // back to the address counts as a frame (its pledged header
        // size is the logical size); anything else is raw, and its
        // stored length IS the logical size. A raw blob that merely
        // looks like a frame therefore reports its own length instead
        // of a bogus pledge from bytes that never decoded.
        let path = self.blob_path(id);
        let stored = match fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let stored_len = stored.len() as u64;
        let size = if crate::compression::is_zstd_frame(&stored) {
            if let Ok(decoded) = crate::compression::decode(&stored) {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(&decoded);
                if hasher.finalize().as_slice() == id.digest() {
                    // A verified frame: the header pledge is trustworthy.
                    logical_size(&stored[..stored.len().min(32)], stored_len)
                } else {
                    // Looks like a frame, hashes like raw: raw it is.
                    stored_len
                }
            } else {
                // Undecodable frame-lookalike: raw length.
                stored_len
            }
        } else {
            stored_len
        };
        Ok(Some(BlobInfo {
            id: id.clone(),
            size,
        }))
    }

    async fn delete(&self, id: &BlobId) -> Result<(), Error> {
        match fs::remove_file(self.blob_path(id)).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_and_dedup_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(root.path());

        let bytes = b"hello content-addressed world".repeat(100);
        let id = backend.put(&mut bytes.as_slice()).await.unwrap().id;
        assert_eq!(backend.get(&id).await.unwrap(), bytes);

        // Same bytes again: idempotent, one stored copy.
        let id2 = backend.put(&mut bytes.as_slice()).await.unwrap().id;
        assert_eq!(id, id2);

        // Range windows: past-end rejected, over-long clamped.
        let window = backend.get_range(&id, 6, 7).await.unwrap();
        assert_eq!(&window, &bytes[6..13]);
        assert!(matches!(
            backend.get_range(&id, bytes.len() as u64, 1).await,
            Err(Error::RangeOutOfBounds { .. })
        ));
        let tail = backend
            .get_range(&id, (bytes.len() - 2) as u64, 99)
            .await
            .unwrap();
        assert_eq!(&tail, &bytes[bytes.len() - 2..]);

        // The committed layout matches the exposed path helper.
        assert!(ingest::blob_path(root.path(), &id).is_file());

        // Delete is idempotent.
        backend.delete(&id).await.unwrap();
        backend.delete(&id).await.unwrap();
        assert_eq!(backend.stat(&id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn zstd_round_trip_hashes_original_bytes() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::with_compression(
            root.path(),
            crate::compression::Compression::Zstd { level: 3 },
        );

        // Compressible payload: the stored frame must differ from the
        // logical bytes while the address stays the ORIGINAL digest.
        let bytes = b"compressible content, repeated to shrink well. ".repeat(64);
        let id = backend.put(&mut bytes.as_slice()).await.unwrap().id;
        assert_eq!(id, BlobId::for_bytes(&bytes));
        assert_eq!(backend.get(&id).await.unwrap(), bytes);

        let stored = std::fs::read(ingest::blob_path(root.path(), &id)).unwrap();
        assert!(crate::compression::is_zstd_frame(&stored));
        assert_ne!(stored, bytes);

        // Range windows decode back to logical-byte offsets.
        let window = backend.get_range(&id, 6, 7).await.unwrap();
        assert_eq!(&window, &bytes[6..13]);

        // stat reports the LOGICAL size, read from the frame header's
        // pledged source size (no decompression).
        let info = backend.stat(&id).await.unwrap().unwrap();
        assert_eq!(info.size, bytes.len() as u64);
        assert_ne!(info.size, stored.len() as u64);
    }

    #[tokio::test]
    async fn legacy_uncompressed_blobs_stay_readable_through_a_zstd_backend() {
        let root = tempfile::tempdir().unwrap();
        let raw = LocalBackend::new(root.path());
        let zstd_backend = LocalBackend::with_compression(
            root.path(),
            crate::compression::Compression::Zstd { level: 3 },
        );

        let bytes = b"written by the historical raw representation".to_vec();
        let id = raw.put(&mut bytes.as_slice()).await.unwrap().id;
        // The same root read through a compression-enabled backend still
        // yields the logical bytes: the frame magic keeps formats apart.
        assert_eq!(zstd_backend.get(&id).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn dedup_keeps_the_incumbent_representation() {
        let root = tempfile::tempdir().unwrap();
        let raw = LocalBackend::new(root.path());
        let zstd_backend = LocalBackend::with_compression(
            root.path(),
            crate::compression::Compression::Zstd { level: 3 },
        );

        let bytes = b"identical content, two representations".repeat(32);
        let first = raw.put(&mut bytes.as_slice()).await.unwrap().id;
        let second = zstd_backend.put(&mut bytes.as_slice()).await.unwrap().id;
        assert_eq!(first, second);

        // The incumbent (raw) copy stays: put is idempotent on the digest,
        // so representation migration belongs to the rewrite command.
        let stored = std::fs::read(ingest::blob_path(root.path(), &first)).unwrap();
        assert!(!crate::compression::is_zstd_frame(&stored));
        assert_eq!(zstd_backend.get(&first).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn tiny_inputs_round_trip_through_zstd() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::with_compression(
            root.path(),
            crate::compression::Compression::Zstd { level: 3 },
        );
        let bytes = b"x".to_vec();
        let id = backend.put(&mut bytes.as_slice()).await.unwrap().id;
        assert_eq!(backend.get(&id).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn the_size_threshold_keeps_large_objects_raw() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::with_policy(
            root.path(),
            crate::store::IngestPolicy {
                compression: crate::compression::Compression::Zstd { level: 3 },
                compress_above: Some(1024),
            },
        );
        // Well past the threshold: the ingest decision must be raw
        // even though the policy compresses, and reads must return the
        // original bytes.
        let big = vec![7u8; 8 * 1024];
        let outcome = backend.put(&mut big.as_slice()).await.unwrap();
        let stored = std::fs::read(ingest::blob_path(root.path(), &outcome.id)).unwrap();
        assert!(!crate::compression::is_zstd_frame(&stored));
        assert_eq!(backend.get(&outcome.id).await.unwrap(), big);

        // Under the threshold: the policy applies and the frame reads
        // back decoded.
        let small = b"small enough to frame".to_vec();
        let outcome = backend.put(&mut small.as_slice()).await.unwrap();
        let stored = std::fs::read(ingest::blob_path(root.path(), &outcome.id)).unwrap();
        assert!(crate::compression::is_zstd_frame(&stored));
        assert_eq!(backend.get(&outcome.id).await.unwrap(), small);
    }

    #[tokio::test]
    async fn a_magic_prefix_does_not_make_a_raw_object_a_frame() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(root.path());
        // Bytes that open with the zstd magic but are not a frame: a
        // read passes them through unchanged. Self-identification
        // needs the decoded content to hash back to the address; a
        // false positive cannot hijack the read.
        let mut bytes = crate::compression::ZSTD_MAGIC.to_vec();
        bytes.extend_from_slice(b"plainly not a frame");
        let outcome = backend.put(&mut bytes.as_slice()).await.unwrap();
        assert_eq!(backend.get(&outcome.id).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_frame_that_hashes_to_another_address_reads_as_raw() {
        // Take a genuine frame and store it under a DIFFERENT blob's
        // address (a hash collision would be needed to do this by
        // accident). Self-identification decodes it, sees the content
        // hash does not come home, and returns the stored bytes
        // unchanged: for this address, raw is what they are.
        let root = tempfile::tempdir().unwrap();
        let framed_store = LocalBackend::with_compression(
            root.path(),
            crate::compression::Compression::Zstd { level: 3 },
        );
        let payload = b"a frame that belongs to someone else".to_vec();
        let real = framed_store
            .put(&mut std::io::Cursor::new(&payload))
            .await
            .unwrap();
        let frame_bytes = std::fs::read(ingest::blob_path(root.path(), &real.id)).unwrap();

        let impostor = LocalBackend::new(root.path());
        let impostor_payload = b"the impostor's true raw content".to_vec();
        let impostor_id = impostor
            .put(&mut std::io::Cursor::new(&impostor_payload))
            .await
            .unwrap()
            .id;
        // Overwrite the impostor's file with the foreign frame.
        std::fs::write(ingest::blob_path(root.path(), &impostor_id), &frame_bytes).unwrap();

        let read_back = impostor.get(&impostor_id).await.unwrap();
        assert_eq!(read_back, frame_bytes, "hash does not come home: raw it is");
    }

    #[tokio::test]
    async fn range_reads_cut_logical_bytes_of_a_frame() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::with_compression(
            root.path(),
            crate::compression::Compression::Zstd { level: 3 },
        );
        let payload = b"self-identified frames drive reads ".repeat(8);
        let outcome = backend.put(&mut payload.as_slice()).await.unwrap();

        // The stored copy is a frame; a range read cuts the LOGICAL
        // window and its bounds are logical too.
        let stored = std::fs::read(ingest::blob_path(root.path(), &outcome.id)).unwrap();
        assert!(crate::compression::is_zstd_frame(&stored));
        let window = backend.get_range(&outcome.id, 8, 7).await.unwrap();
        assert_eq!(&window, &payload[8..15]);
        assert!(matches!(
            backend
                .get_range(&outcome.id, payload.len() as u64, 1)
                .await,
            Err(Error::RangeOutOfBounds { .. })
        ));
    }
}
