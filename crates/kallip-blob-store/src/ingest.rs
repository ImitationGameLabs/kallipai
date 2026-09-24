//! Single-pass ingest: hash and write in one stream, then commit by
//! rename. With compression enabled, the raw staging file is re-encoded
//! into a zstd frame (source size pledged in the frame header) before
//! the commit; the address still hashes the ORIGINAL bytes.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::compression::Compression;
use crate::error::Error;
use crate::hash::BlobId;
use crate::store::PutOutcome;

/// Bytes per ingest read. One buffer bounds peak memory no matter how
/// large the upload is; 64 KiB keeps syscall count low without
/// pressuring the allocator.
const CHUNK: usize = 64 * 1024;

/// Stage `content` into a fresh temp file under `<root>/tmp/` -- same
/// volume as the blobs, so the final rename is atomic -- hashing on the
/// way, then commit: if the addressed blob already exists (same digest
/// means same bytes) the temp file is removed and the incumbent stays;
/// otherwise the temp file is renamed into `blobs/`. Nothing partial is
/// ever visible under `blobs/`, and any failure removes the temp file
/// best-effort.
///
/// Concurrent uploads of the same content each stage their own temp
/// file: the first rename wins, the rest see the target and clean up
/// after themselves.
///
/// With [`Compression::Zstd`], the staged raw bytes are re-encoded into
/// a zstd frame whose header pledges the source size (so `stat` can
/// report the logical size without decompressing) and carries a
/// checksum; the frame, not the raw bytes, is what gets committed.
pub(crate) async fn ingest(
    root: &Path,
    content: &mut (dyn AsyncRead + Unpin + Send),
    compression: Compression,
    compress_above: Option<u64>,
) -> Result<PutOutcome, Error> {
    fs::create_dir_all(tmp_dir(root)).await?;

    let tmp_path = fresh_tmp_path(root)?;
    let mut hasher = Sha256::new();
    let mut source_len: u64 = 0;
    let mut tmp = fs::File::create(&tmp_path).await?;
    let mut buf = vec![0u8; CHUNK];
    let staged = async {
        loop {
            let n = content.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            source_len += n as u64;
            tmp.write_all(&buf[..n]).await?;
        }
        tmp.flush().await?;
        tmp.sync_all().await?;
        Ok::<(), Error>(())
    }
    .await;
    if let Err(err) = staged {
        let _ = fs::remove_file(&tmp_path).await;
        return Err(err);
    }

    // The representation decision belongs to the ingest path alone:
    // compress only when the policy asks for it AND the object fits
    // under the configured size threshold (None = no threshold). Above
    // the threshold the object stays raw, which keeps the files
    // service's peak-memory invariant (one window, never a whole frame
    // decode) for exactly the objects where decoding hurts.
    let decided = match compression {
        Compression::Zstd { .. } => compress_above.is_none_or(|limit| source_len <= limit),
        Compression::Off => false,
    };

    // Compress the staged raw bytes into a second temp file, swapping
    // paths so the commit below renames the frame. The encoder pledges
    // the exact source size, which lands in the frame header.
    let committed_path = match decided {
        false => tmp_path,
        true => {
            let Compression::Zstd { level } = compression else {
                unreachable!("decided true only under a zstd policy")
            };
            match encode_tmp_file(&tmp_path, root, level).await {
                Ok(encoded) => {
                    let _ = fs::remove_file(&tmp_path).await;
                    encoded
                }
                // Both temps are ours: drop the partial frame and the
                // raw staging file rather than leaking them under tmp/.
                Err(err) => {
                    let _ = fs::remove_file(&tmp_path).await;
                    return Err(err);
                }
            }
        }
    };

    let digest = finalize_sha256(&mut hasher);
    let id = BlobId::from_digest(digest);

    let target = blob_path(root, &id);
    // The bucket directory is created before the existence check so a
    // first-of-its-bucket rename cannot fail on a missing parent (that
    // error would surface as a bare ENOENT, indistinguishable from the
    // blob-missing case for callers).
    fs::create_dir_all(target.parent().expect("blob path always has a parent")).await?;
    match fs::metadata(&target).await {
        // Same digest is same bytes: the stored copy is authoritative.
        Ok(_) => {
            let _ = fs::remove_file(&committed_path).await;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Err(rename_err) = fs::rename(&committed_path, &target).await {
                let _ = fs::remove_file(&committed_path).await;
                return Err(rename_err.into());
            }
        }
        Err(err) => {
            let _ = fs::remove_file(&committed_path).await;
            return Err(err.into());
        }
    }
    Ok(PutOutcome { id })
}

/// Re-encode a fully staged raw temp file into a zstd frame: the source
/// size is pledged (frame header) and a checksum is included, so readers
/// can stat the logical size from the header and verify integrity on
/// decode. Blocking disk work, so it runs on the blocking pool.
async fn encode_tmp_file(src: &Path, root: &Path, level: i32) -> Result<PathBuf, Error> {
    let src = src.to_owned();
    let root = root.to_owned();
    // Blocking disk + CPU work: keep it off the async executor. The
    // source length is read inside the closure from the already-open
    // file, and every failure after the temp file exists removes it.
    tokio::task::spawn_blocking(move || -> Result<PathBuf, Error> {
        let dst = fresh_tmp_path(&root)?;
        // The closure only borrows the staged path; ownership stays
        // outside so the cleanup below can always remove it.
        let outcome = (|| -> Result<(), Error> {
            let mut input = std::fs::File::open(&src)?;
            let source_len = input.metadata()?.len();
            crate::encode::encode_frame_into(&dst, &mut input, source_len, level)?;
            Ok(())
        })();
        match outcome {
            Ok(()) => Ok(dst),
            Err(err) => {
                let _ = std::fs::remove_file(&dst);
                Err(err)
            }
        }
    })
    .await
    .map_err(|e| Error::Encode(format!("encode task panicked: {e}")))?
}

/// `<root>/blobs/<first two hex characters>/<id>`.
pub fn blob_path(root: &Path, id: &BlobId) -> PathBuf {
    root.join("blobs").join(id.bucket()).join(id.as_str())
}

fn tmp_dir(root: &Path) -> PathBuf {
    root.join("tmp")
}

/// A fresh random temp file name: pure randomness with no content
/// semantics, so concurrent ingests never stage into the same file.
/// 128 bits makes a stray collision negligible; no retry loop to test.
pub(crate) fn fresh_tmp_path(root: &Path) -> io::Result<PathBuf> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(tmp_dir(root).join(hex::encode(bytes)))
}

fn finalize_sha256(hasher: &mut Sha256) -> [u8; 32] {
    let digest = hasher.finalize_reset();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}
