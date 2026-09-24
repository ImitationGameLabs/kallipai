//! Ingest behavior: id encoding, idempotent re-upload, concurrent
//! same-content staging, dedup, and delete semantics.

use std::path::Path;
use std::sync::Arc;

use kallip_files::{BlobId, BlobStore, Error, LocalBackend};

/// The SHA-256 of the empty string, pinned from the published digest.
const EMPTY_SHA256: &str =
    "sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const CONTENT: &[u8] = b"the quick brown fox jumps over the lazy dog";

fn reader(bytes: &'static [u8]) -> std::io::Cursor<&'static [u8]> {
    std::io::Cursor::new(bytes)
}

async fn store() -> (LocalBackend, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    (LocalBackend::new(dir.path()), dir)
}

/// Counts files (not directories) under `dir`, recursively.
fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        if entry.file_type().expect("file type").is_dir() {
            count += count_files(&entry.path());
        } else {
            count += 1;
        }
    }
    count
}

#[test]
fn parse_accepts_the_canonical_form_and_rejects_the_rest() {
    assert!(BlobId::parse(EMPTY_SHA256).is_ok());
    for bad in [
        // Missing prefix.
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        // Wrong algorithm prefix.
        "md5-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        // 63 and 65 hex characters.
        "sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
        "sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8550",
        // Upper case is not canonical.
        "sha256-E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
        // Non-hex character.
        "sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85g",
    ] {
        assert!(
            matches!(BlobId::parse(bad), Err(Error::InvalidId(_))),
            "{bad}"
        );
    }
}

/// Uploading the empty stream pins the exact known digest vector.
#[tokio::test]
async fn empty_upload_yields_the_known_digest_id() {
    let (backend, _dir) = store().await;
    let id = backend.put(&mut reader(b"")).await.expect("put").id;
    assert_eq!(id.as_str(), EMPTY_SHA256);
    let info = backend.stat(&id).await.expect("stat").expect("stored");
    assert_eq!(info.size, 0);
}

/// Same bytes, same id; the re-upload leaves exactly one stored copy
/// and no leftover staging files.
#[tokio::test]
async fn reupload_is_idempotent_with_one_stored_copy() {
    let (backend, dir) = store().await;
    let first = backend.put(&mut reader(CONTENT)).await.expect("put").id;
    let second = backend.put(&mut reader(CONTENT)).await.expect("put").id;
    assert_eq!(first, second);
    assert_eq!(count_files(&dir.path().join("blobs")), 1);
    assert_eq!(count_files(&dir.path().join("tmp")), 0);
    let info = backend.stat(&first).await.expect("stat").expect("stored");
    assert_eq!(info.size, CONTENT.len() as u64);
    assert_eq!(backend.get(&first).await.expect("get"), CONTENT);
}

/// Distinct bytes address distinct blobs.
#[tokio::test]
async fn distinct_content_yields_distinct_ids() {
    let (backend, _dir) = store().await;
    let a = backend.put(&mut reader(b"alpha")).await.expect("put a").id;
    let b = backend.put(&mut reader(b"beta")).await.expect("put b").id;
    assert_ne!(a, b);
    assert_eq!(backend.stat(&a).await.expect("stat").unwrap().size, 5);
    assert_eq!(backend.stat(&b).await.expect("stat").unwrap().size, 4);
}

/// Concurrent same-content uploads each stage their own temp file: one
/// rename wins, the losers see the target and self-clean, `tmp/` ends
/// empty and `blobs/` keeps a single copy.
#[tokio::test]
async fn concurrent_same_content_uploads_self_clean() {
    let (backend, dir) = store().await;
    let backend = Arc::new(backend);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let backend = backend.clone();
        tasks.push(tokio::spawn(async move {
            backend.put(&mut reader(CONTENT)).await
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        ids.push(task.await.expect("join").expect("put"));
    }
    assert!(ids.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(count_files(&dir.path().join("blobs")), 1);
    assert_eq!(count_files(&dir.path().join("tmp")), 0);
}

/// Delete removes the blob and is idempotent; reads after delete miss
/// with NotFound.
#[tokio::test]
async fn delete_is_idempotent_and_reads_then_miss() {
    let (backend, _dir) = store().await;
    let id = backend
        .put(&mut reader(b"ephemeral"))
        .await
        .expect("put")
        .id;
    assert!(backend.stat(&id).await.expect("stat").is_some());
    backend.delete(&id).await.expect("delete");
    backend.delete(&id).await.expect("delete again");
    assert!(backend.stat(&id).await.expect("stat").is_none());
    assert!(matches!(backend.get(&id).await, Err(Error::NotFound(_))));
}

/// The stored file lives at `blobs/<first two hex characters>/<id>`.
#[tokio::test]
async fn stored_file_lands_in_its_bucket() {
    let (backend, dir) = store().await;
    let id = backend.put(&mut reader(CONTENT)).await.expect("put").id;
    let path = dir.path().join("blobs").join(id.bucket()).join(id.as_str());
    assert!(path.is_file());
}

/// Content several times the 64 KiB staging buffer exercises the
/// multi-chunk read loop: bytes round-trip exactly, the size is
/// right, and a re-upload deduplicates to the same id.
#[tokio::test]
async fn multi_chunk_content_roundtrips_and_dedups() {
    let (backend, _dir) = store().await;
    let unit = b"kallip-files chunk boundary. ";
    let mut content = Vec::new();
    while content.len() < 3 * 64 * 1024 + 17 {
        content.extend_from_slice(unit);
    }
    let expected_len = content.len() as u64;
    let id = backend
        .put(&mut std::io::Cursor::new(content.clone()))
        .await
        .expect("put")
        .id;
    assert_eq!(backend.get(&id).await.expect("get"), content);
    let info = backend.stat(&id).await.expect("stat").expect("stored");
    assert_eq!(info.size, expected_len);
    let again = backend
        .put(&mut std::io::Cursor::new(content))
        .await
        .expect("re-put")
        .id;
    assert_eq!(id, again);
}
