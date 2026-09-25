//! Range semantics: head out-of-bounds errors, tail clamps to EOF,
//! middle windows slice exactly, missing blobs report NotFound.

use kallipai_files::{BlobId, BlobStore, Error, LocalBackend};

const CONTENT: &[u8] = b"0123456789";

async fn stored() -> (LocalBackend, tempfile::TempDir, BlobId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = LocalBackend::new(dir.path());
    let id = backend
        .put(&mut std::io::Cursor::new(CONTENT))
        .await
        .expect("put")
        .id;
    (backend, dir, id)
}

/// An offset at or past the end is unsatisfiable (the HTTP seam maps
/// this to 416).
#[tokio::test]
async fn offset_at_or_past_end_is_out_of_bounds() {
    let (backend, _dir, id) = stored().await;
    assert!(matches!(
        backend.get_range(&id, CONTENT.len() as u64, 0).await,
        Err(Error::RangeOutOfBounds { .. })
    ));
    assert!(matches!(
        backend.get_range(&id, CONTENT.len() as u64 + 1, 5).await,
        Err(Error::RangeOutOfBounds { .. })
    ));
}

/// A len running past the end clamps to EOF: open-ended ranges must
/// succeed.
#[tokio::test]
async fn len_past_end_clamps_to_eof() {
    let (backend, _dir, id) = stored().await;
    assert_eq!(
        backend.get_range(&id, 8, 100).await.expect("clamped range"),
        b"89"
    );
}

/// A middle window reads exactly the requested slice.
#[tokio::test]
async fn middle_window_reads_the_slice() {
    let (backend, _dir, id) = stored().await;
    assert_eq!(backend.get_range(&id, 3, 4).await.expect("range"), b"3456");
}

/// The full range matches a whole-blob read.
#[tokio::test]
async fn full_range_matches_whole_read() {
    let (backend, _dir, id) = stored().await;
    assert_eq!(
        backend
            .get_range(&id, 0, CONTENT.len() as u64)
            .await
            .expect("range"),
        backend.get(&id).await.expect("get")
    );
}

/// A zero-length window at a valid offset reads empty.
#[tokio::test]
async fn zero_len_at_valid_offset_reads_empty() {
    let (backend, _dir, id) = stored().await;
    assert!(
        backend
            .get_range(&id, 4, 0)
            .await
            .expect("range")
            .is_empty()
    );
}

/// Ranges against a missing blob report NotFound, not raw IO errors.
#[tokio::test]
async fn missing_blob_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = LocalBackend::new(dir.path());
    let id =
        BlobId::parse("sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            .expect("valid id");
    assert!(matches!(
        backend.get_range(&id, 0, 1).await,
        Err(Error::NotFound(_))
    ));
    assert!(backend.stat(&id).await.expect("stat").is_none());
}
