//! Offline representation rewrite: walk a LocalBackend root and re-encode
//! every raw blob into a zstd frame.
//!
//! Safety contract, per blob: read -> verify the sha256 against the
//! address (the name IS the integrity anchor) -> encode (source size
//! pledged in the frame header, checksum on) -> decode round-trip and
//! compare byte-for-byte -> stage via temp file + fsync -> atomic rename.
//! Already-compressed blobs are skipped, which is what makes an
//! interrupted run safely resumable; a blob whose bytes no longer hash
//! to its address stops the run (or is counted, under `Skip`), never
//! silently rewritten.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::compression::{decode, is_zstd_frame};
use crate::hash::BlobId;
use crate::ingest::fresh_tmp_path;

/// What to do when a blob's contents no longer hash to its address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorruptPolicy {
    /// Stop the whole run (the default): corruption wants a human.
    #[default]
    Fail,
    /// Leave the blob untouched, count it, keep going.
    Skip,
}

/// Knobs for one rewrite pass.
#[derive(Debug, Clone)]
pub struct RewriteOptions {
    /// Report what would change without touching anything.
    pub dry_run: bool,
    /// zstd level for newly encoded frames.
    pub level: i32,
    /// Corruption handling.
    pub corrupt: CorruptPolicy,
    /// Emit a progress line after this many blobs (0 = quiet).
    pub progress_every: usize,
    /// Raw blobs whose stored size exceeds this many bytes stay raw
    /// (policy alignment with the writer's size threshold; `None` =
    /// no limit). Frames are never size-filtered: a stored frame is
    /// already in its final representation.
    pub max_stored_bytes: Option<u64>,
}

impl Default for RewriteOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            level: 3,
            corrupt: CorruptPolicy::Fail,
            progress_every: 0,
            max_stored_bytes: None,
        }
    }
}

/// Counters from one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewriteReport {
    pub scanned: usize,
    pub already_compressed: usize,
    pub rewritten: usize,
    pub skipped_corrupt: usize,
    /// Raw blobs over the size threshold, left raw on purpose.
    pub skipped_oversized: usize,
    pub dry_run_would_rewrite: usize,
    /// Bytes currently stored raw (before the pass; unchanged by it).
    pub bytes_raw: u64,
    /// Bytes currently stored as frames, including every frame this
    /// pass committed.
    pub bytes_compressed: u64,
}

/// Walk `<root>/blobs/<bucket>/<id>` and re-encode raw blobs. Returns
/// `Err` on the first operational failure (IO / encode) or on a corrupt
/// blob under [`CorruptPolicy::Fail`]; whatever was already committed
/// stays committed.
pub fn rewrite_root(root: &Path, opts: &RewriteOptions) -> io::Result<RewriteReport> {
    let mut report = RewriteReport::default();
    let blobs_dir = root.join("blobs");
    let mut buckets: Vec<_> = std::fs::read_dir(&blobs_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    buckets.sort();
    for bucket in buckets {
        // Per-bucket dirty tracking: one directory fsync per changed bucket
        // instead of one per rename. What a power loss can steal from a
        // rename is its directory entry, so the bucket fsync is what makes
        // the batch durable -- at O(buckets) syncs, not O(blobs).
        let mut bucket_dirty = false;
        let mut entries: Vec<_> = std::fs::read_dir(&bucket)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();
        for path in entries {
            report.scanned += 1;
            let stored = std::fs::read(&path)?;

            // Already a zstd frame: representation migration is done here.
            if is_zstd_frame(&stored) {
                report.already_compressed += 1;
                report.bytes_compressed += stored.len() as u64;
            } else {
                report.bytes_raw += stored.len() as u64;

                // Size-threshold alignment: oversized raw blobs stay
                // raw, exactly as the writer's policy would keep them.
                if let Some(max) = opts.max_stored_bytes
                    && stored.len() as u64 > max
                {
                    report.skipped_oversized += 1;
                    continue;
                }

                // The address IS the integrity anchor: verify before
                // rewriting, never rewrite bytes we cannot vouch for.
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| unrecognized_name(&path))?;
                let id = BlobId::parse(name).map_err(|_| unrecognized_name(&path))?;
                let mut hasher = Sha256::new();
                hasher.update(&stored);
                if hasher.finalize().as_slice() != id.digest() {
                    match opts.corrupt {
                        CorruptPolicy::Fail => return Err(corrupt(&path)),
                        CorruptPolicy::Skip => {
                            report.skipped_corrupt += 1;
                            continue;
                        }
                    }
                }

                if opts.dry_run {
                    report.dry_run_would_rewrite += 1;
                } else {
                    let staged = encode_staged(&stored, root, opts.level)?;
                    // Round-trip proof before the commit rename: the
                    // frame must decode to exactly the verified bytes.
                    let committed = std::fs::read(&staged)?;
                    debug_assert!(is_zstd_frame(&committed));
                    if decode(&committed)?.as_slice() != stored {
                        let _ = std::fs::remove_file(&staged);
                        return Err(io::Error::other(format!("round-trip mismatch for {name}")));
                    }
                    // Atomic commit, same volume (root/tmp); marks the
                    // bucket for its end-of-bucket directory fsync.
                    std::fs::rename(&staged, &path)?;
                    bucket_dirty = true;
                    report.bytes_compressed += committed.len() as u64;
                    report.rewritten += 1;
                }
            }

            if opts.progress_every > 0 && report.scanned % opts.progress_every == 0 {
                eprintln!(
                    "[blobs rewrite] scanned={} already_compressed={} rewritten={} skipped_corrupt={}",
                    report.scanned,
                    report.already_compressed,
                    report.rewritten,
                    report.skipped_corrupt
                );
            }
        }
        if bucket_dirty {
            let dir = std::fs::File::open(&bucket)?;
            dir.sync_all()?;
        }
    }
    Ok(report)
}

/// A blob whose contents no longer hash to its address: the stored
/// copy cannot be vouched for, so it is never rewritten.
fn corrupt(path: &Path) -> io::Error {
    io::Error::other(format!(
        "stored blob contents do not hash to their address: {}",
        path.display()
    ))
}

/// A file under blobs/ whose name is not a valid blob id: not
/// corruption of a known blob, but something this pass must not touch.
fn unrecognized_name(path: &Path) -> io::Error {
    io::Error::other(format!(
        "unrecognized file name under blobs/ (not a blob id): {}",
        path.display()
    ))
}

/// Stage the frame for `stored` under `<root>/tmp/` via the shared
/// encode pass, then fsync it. The caller commits by rename and owns
/// removing the staged file when this fails.
fn encode_staged(stored: &[u8], root: &Path, level: i32) -> io::Result<PathBuf> {
    let staged = fresh_tmp_path(root)?;
    let mut cursor = io::Cursor::new(stored);
    if let Err(err) =
        crate::encode::encode_frame_into(&staged, &mut cursor, stored.len() as u64, level)
    {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::compression::Compression;
    use crate::error::Error;
    use crate::ingest::blob_path;
    use crate::local::LocalBackend;
    use crate::store::BlobStore;

    async fn seed(
        root: &Path,
        mut raw_payload: &[u8],
        mut zstd_payload: &[u8],
    ) -> (LocalBackend, BlobId, BlobId) {
        let raw = LocalBackend::new(root);
        let zstd = LocalBackend::with_compression(root, Compression::Zstd { level: 3 });
        let raw_id = raw.put(&mut raw_payload).await.unwrap().id;
        let zstd_id = zstd.put(&mut zstd_payload).await.unwrap().id;
        (raw, raw_id, zstd_id)
    }

    fn opts() -> RewriteOptions {
        RewriteOptions {
            progress_every: 0,
            ..RewriteOptions::default()
        }
    }

    #[tokio::test]
    async fn rewrite_compresses_raw_and_keeps_bytes_identical() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"rewrite me: compressible text ".repeat(64);
        let zstd_payload = b"already a frame".to_vec();
        let (_backend, raw_id, zstd_id) = seed(root.path(), &raw_payload, &zstd_payload).await;

        let report = rewrite_root(root.path(), &opts()).unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.already_compressed, 1);
        assert_eq!(report.rewritten, 1);
        // The frame replaces the raw blob, so compressed bytes must
        // shrink relative to the raw bytes the pass found.
        assert!(report.bytes_compressed < report.bytes_raw);
        assert_eq!(report.bytes_raw, raw_payload.len() as u64);

        // Addresses unchanged; logical bytes unchanged.
        let zstd_backend =
            LocalBackend::with_compression(root.path(), Compression::Zstd { level: 3 });
        let got_raw = zstd_backend.get(&raw_id).await.unwrap();
        assert_eq!(got_raw, raw_payload);
        let got_frame = zstd_backend.get(&zstd_id).await.unwrap();
        assert_eq!(got_frame, zstd_payload);

        // Second pass: everything already compressed, nothing to do.
        let again = rewrite_root(root.path(), &opts()).unwrap();
        assert_eq!(again.already_compressed, 2);
        assert_eq!(again.rewritten, 0);
    }

    #[tokio::test]
    async fn dry_run_touches_nothing() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"dry run payload ".repeat(64);
        let zstd_payload = b"frame".to_vec();
        let (_backend, raw_id, _) = seed(root.path(), &raw_payload, &zstd_payload).await;
        let before = std::fs::read(blob_path(root.path(), &raw_id)).unwrap();

        let mut o = opts();
        o.dry_run = true;
        let report = rewrite_root(root.path(), &o).unwrap();
        assert_eq!(report.dry_run_would_rewrite, 1);
        assert_eq!(report.rewritten, 0);
        let after = std::fs::read(blob_path(root.path(), &raw_id)).unwrap();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn corrupt_blob_fails_the_run_under_fail_policy() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"about to be corrupted".to_vec();
        let zstd_payload = b"frame".to_vec();
        let (_backend, raw_id, _) = seed(root.path(), &raw_payload, &zstd_payload).await;

        // Tamper after the address was minted: contents no longer hash
        // to the id.
        let path = blob_path(root.path(), &raw_id);
        std::fs::write(&path, b"tampered bytes").unwrap();

        let err = rewrite_root(root.path(), &opts()).unwrap_err();
        assert!(err.to_string().contains("do not hash to their address"));
    }

    #[tokio::test]
    async fn corrupt_blob_is_counted_under_skip_policy() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"kept as-is despite corruption ".repeat(32);
        let zstd_payload = b"frame".to_vec();
        let (_backend, raw_id, zstd_id) = seed(root.path(), &raw_payload, &zstd_payload).await;
        let path = blob_path(root.path(), &raw_id);
        std::fs::write(&path, b"tampered bytes").unwrap();

        let mut o = opts();
        o.corrupt = CorruptPolicy::Skip;
        let report = rewrite_root(root.path(), &o).unwrap();
        assert_eq!(report.skipped_corrupt, 1);
        assert_eq!(report.rewritten, 0); // the only healthy blob is already a frame
        assert_eq!(report.already_compressed, 1);
        // The corrupt blob was left exactly as found.
        assert_eq!(std::fs::read(&path).unwrap(), b"tampered bytes".to_vec());
        let _ = zstd_id;
    }

    #[tokio::test]
    async fn rewritten_blobs_report_logical_size_and_range_after_the_pass() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"stat and range must survive the rewrite ".repeat(48);
        let zstd_payload = b"frame".to_vec();
        let (raw_backend, raw_id, _) = seed(root.path(), &raw_payload, &zstd_payload).await;
        let zstd_backend =
            LocalBackend::with_compression(root.path(), Compression::Zstd { level: 3 });

        // Before: raw file, logical == stored.
        assert_eq!(
            raw_backend.stat(&raw_id).await.unwrap().unwrap().size,
            raw_payload.len() as u64
        );
        rewrite_root(root.path(), &opts()).unwrap();

        // After: stat reads the pledged size from the frame header, and
        // ranged reads serve the same logical window as before.
        let info = zstd_backend.stat(&raw_id).await.unwrap().unwrap();
        assert_eq!(info.size, raw_payload.len() as u64);
        let window = zstd_backend.get_range(&raw_id, 8, 6).await.unwrap();
        assert_eq!(&window, &raw_payload[8..14]);
        assert!(matches!(
            zstd_backend
                .get_range(&raw_id, raw_payload.len() as u64, 1)
                .await,
            Err(Error::RangeOutOfBounds { .. })
        ));
    }

    #[tokio::test]
    async fn level_actually_changes_the_stored_size() {
        // Separate roots: same id means the second put would hit the
        // dedup incumbent and never reach the encoder. The payload is
        // half pseudo-random noise, half zeros -- random stretches cap
        // how small any level can get, while the zero stretches give
        // higher levels room to find longer matches, which is where
        // the level knob moves the stored size.
        let root_low = tempfile::tempdir().unwrap();
        let root_high = tempfile::tempdir().unwrap();
        let mut payload: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut state = 0x1234_5678_u32;
        for _ in 0..32 * 1024 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            payload.push((state >> 24) as u8);
        }
        payload.resize(payload.len() + 32 * 1024, 0u8);
        let low = LocalBackend::with_compression(root_low.path(), Compression::Zstd { level: 1 });
        let high =
            LocalBackend::with_compression(root_high.path(), Compression::Zstd { level: 19 });
        let id_low = low.put(&mut payload.as_slice()).await.unwrap().id;
        let id_high = high.put(&mut payload.as_slice()).await.unwrap().id;
        assert_eq!(id_low, id_high);
        let low_len = std::fs::read(blob_path(root_low.path(), &id_low))
            .unwrap()
            .len();
        let high_len = std::fs::read(blob_path(root_high.path(), &id_high))
            .unwrap()
            .len();
        // Distinct levels must produce distinct frames for the same
        // input, otherwise the level knob has no observable effect.
        assert_ne!(low_len, high_len);
    }

    #[tokio::test]
    async fn compressed_blob_range_out_of_bounds_reports_logical_size() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::with_compression(root.path(), Compression::Zstd { level: 3 });
        let payload = b"out of bounds probe over a frame ".repeat(16);
        let id = backend.put(&mut payload.as_slice()).await.unwrap().id;
        // The contract bounds are logical, so the error carries the
        // logical size even though the file is smaller.
        match backend.get_range(&id, payload.len() as u64, 1).await {
            Err(Error::RangeOutOfBounds { size, .. }) => {
                assert_eq!(size, payload.len() as u64);
            }
            other => panic!("expected RangeOutOfBounds, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_raw_blobs_are_skipped_and_counted() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"healthy raw under the threshold ".repeat(4);
        let big_payload: Vec<u8> = (0..8192u32).map(|i| (i * 7 % 251) as u8).collect();
        let zstd_payload = b"frame".to_vec();
        let (backend, _raw_id, _) = seed(root.path(), &raw_payload, &zstd_payload).await;
        let big = backend
            .put(&mut std::io::Cursor::new(&big_payload))
            .await
            .unwrap()
            .id;

        // Threshold between the two raw sizes: only the big one is
        // oversized, and the corrupt-check must never run for it (the
        // bytes are fine, just too big to re-frame).
        let opts = RewriteOptions {
            max_stored_bytes: Some(raw_payload.len() as u64 + 16),
            corrupt: CorruptPolicy::Skip,
            ..Default::default()
        };
        let report = rewrite_root(root.path(), &opts).unwrap();
        assert_eq!(report.skipped_oversized, 1);
        assert_eq!(report.rewritten, 1, "the under-threshold blob still frames");
        assert!(!crate::compression::is_zstd_frame(
            &std::fs::read(blob_path(root.path(), &big)).unwrap()
        ));
    }

    #[tokio::test]
    async fn oversized_corrupt_blob_stays_oversized_not_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
        let zstd_payload = b"frame".to_vec();
        let (_backend, raw_id, _) = seed(root.path(), &raw_payload, &zstd_payload).await;
        // Corrupt the raw blob AND make it oversized: the size filter
        // fires first, so the pass succeeds even under Fail policy.
        let path = blob_path(root.path(), &raw_id);
        std::fs::write(&path, vec![0u8; 8192]).unwrap();

        let opts = RewriteOptions {
            max_stored_bytes: Some(1024),
            corrupt: CorruptPolicy::Fail,
            ..Default::default()
        };
        let report = rewrite_root(root.path(), &opts).unwrap();
        assert_eq!(report.skipped_oversized, 1);
        assert_eq!(report.skipped_corrupt, 0);
    }

    #[tokio::test]
    async fn empty_blob_round_trips_through_compression() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::with_compression(root.path(), Compression::Zstd { level: 3 });
        let payload: Vec<u8> = Vec::new();
        let id = backend.put(&mut payload.as_slice()).await.unwrap().id;
        assert_eq!(id, BlobId::for_bytes(&payload));
        assert_eq!(backend.get(&id).await.unwrap(), payload);
    }

    #[tokio::test]
    async fn unrecognized_file_name_under_blobs_fails_the_pass() {
        let root = tempfile::tempdir().unwrap();
        let raw_payload = b"healthy blob so the pass reaches scan ".repeat(16);
        let zstd_payload = b"frame".to_vec();
        let (_, raw_id, _) = seed(root.path(), &raw_payload, &zstd_payload).await;
        // A name that parses as nothing in particular, dropped into a
        // real bucket: not corruption of a known blob (there is no
        // known blob behind it), so the CorruptPolicy must not govern
        // it -- the pass refuses outright.
        let bucket_dir = blob_path(root.path(), &raw_id).parent().unwrap().to_owned();
        std::fs::write(bucket_dir.join("notes.txt"), b"?").unwrap();
        let mut o = opts();
        o.corrupt = CorruptPolicy::Skip;
        let err = rewrite_root(root.path(), &o).unwrap_err();
        assert!(
            err.to_string()
                .contains("unrecognized file name under blobs/ (not a blob id)")
        );
    }
}
