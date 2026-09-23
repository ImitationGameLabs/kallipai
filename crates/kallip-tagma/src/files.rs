//! Files-service access for the tagma process: fetching stored media with
//! the tagma's own credentials. Shared by the attachment-ingest route
//! (live reads) and the restore path (re-assembly after a restart).

use kallip_common::protocol::ApiError;

/// Fetch the record's bytes from the files service under the tagma's
/// own registered credential (`token`, the primary entry's stored
/// enrollment). Whole-body: the service caps uploads, so a stream would
/// add plumbing without changing the memory story.
pub(crate) async fn fetch_record_bytes(
    http: &reqwest::Client,
    token: Option<&str>,
    record_id: uuid::Uuid,
) -> Result<Vec<u8>, ApiError> {
    let token = token.ok_or_else(|| {
        ApiError::unavailable("no registered files credential; the tagma cannot fetch media")
    })?;
    let base = std::env::var("KALLIP_POLIS_URL").map_err(|_| {
        ApiError::unavailable("KALLIP_POLIS_URL is not set; the tagma cannot fetch media")
    })?;
    let response = http
        .get(format!("{base}/v1/files/{record_id}"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| ApiError::unavailable(format!("files fetch failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(files_fetch_error(record_id, status));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| ApiError::unavailable(format!("files read failed: {e}")))
}

/// Map a files-service failure status: only a missing record means the
/// caller named something that does not exist; any other status is an
/// upstream fault (credentials, service health) surfaced as a gateway
/// error that keeps the upstream status for triage.
pub(crate) fn files_fetch_error(record_id: uuid::Uuid, status: reqwest::StatusCode) -> ApiError {
    if status == reqwest::StatusCode::NOT_FOUND {
        ApiError::not_found(format!("files record {record_id} does not exist"))
    } else {
        ApiError::bad_gateway(format!(
            "files fetch for {record_id}: upstream HTTP {status}"
        ))
    }
}

/// Mirror write-through (record-id form only): a hash-addressed copy of
/// media bytes in the tagma data area's attachment store. Fail-open by
/// design -- the record form's master copy lives in the files service, so
/// a mirror failure never blocks the ingest; the reference then carries
/// no blob id and restore falls back to the files service. (The path
/// form is the opposite: its blob is the master copy, fail-closed.)
pub(crate) async fn store_mirror(
    blobs: Option<&std::sync::Arc<dyn kallip_blob_store::BlobStore>>,
    bytes: &[u8],
) -> Option<String> {
    let blobs = blobs?;
    match blobs.put(&mut std::io::Cursor::new(bytes)).await {
        Ok(id) => Some(id.as_str().to_owned()),
        Err(e) => {
            tracing::warn!("attachment mirror write failed (falling back to files): {e}");
            None
        }
    }
}

/// The re-assembly fetch for one reference, local copy first. A
/// reference carrying a blob id is served from the tagma data area's
/// attachment store when the copy exists (zero network, zero files
/// dependency); only a missing copy reaches the files service, whose
/// bytes are then written back for the next restore. `Gone` (a files
/// 404) stays deterministic -- the reference is invalidated once and
/// skipped from then on -- but local-first ordering makes it reachable
/// only when the copy is absent too, so "record deleted, copy retained"
/// holds by construction. A local-read failure other than missing (IO)
/// is transient on its own: it never reaches the files service, so an
/// unrelated local error cannot invalidate a copy that still exists.
/// Everything else (credentials, 5xx, network) is transient the usual
/// way -- the restore stays text-only for that reference and the next
/// boot tries again.
///
/// A path-form reference (a local-blob ingest) carries the nil record
/// id: there is no files record behind it, so once the local copy is
/// missing too the verdict is `Gone` -- the files fetch would be a
/// guaranteed 404 against the nil id.
pub(crate) async fn fetch_local_first(
    blobs: Option<&std::sync::Arc<dyn kallip_blob_store::BlobStore>>,
    record_id: uuid::Uuid,
    bytes_fetch: impl std::future::Future<Output = Result<Vec<u8>, ApiError>>,
    blob_id: Option<&str>,
) -> kallip_runtime::context::FetchedImage {
    use kallip_runtime::context::FetchedImage;
    if let (Some(blobs), Some(anchor)) = (blobs, blob_id)
        && let Ok(id) = kallip_blob_store::BlobId::parse(anchor)
    {
        match blobs.get(&id).await {
            Ok(bytes) => return FetchedImage::Bytes(bytes),
            // A missing copy is the normal fall-through to the files
            // service. Any other local-read failure (IO) is transient in
            // its own right and must not reach the files service below:
            // a 404 there would turn an unrelated local error into a
            // bogus Gone for a copy that still exists.
            Err(kallip_blob_store::Error::NotFound(_)) => {}
            Err(e) => {
                tracing::warn!("attachment mirror read failed: {e}");
                return FetchedImage::Transient(e.to_string());
            }
        }
    }
    // A path-form reference has no files record behind it: after a blob
    // miss the verdict is Gone, and reaching the files service would be
    // a guaranteed 404 against the nil id.
    if record_id.is_nil() {
        return FetchedImage::Gone;
    }
    match bytes_fetch.await {
        Ok(bytes) => {
            if let Some(blobs) = blobs
                && let Err(e) = blobs.put(&mut std::io::Cursor::new(&bytes)).await
            {
                tracing::warn!("attachment mirror backfill failed: {e}");
            }
            FetchedImage::Bytes(bytes)
        }
        Err(e) if e.status == 404 => FetchedImage::Gone,
        Err(e) => FetchedImage::Transient(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_fetch_error_keeps_404_and_folds_the_rest_into_bad_gateway() {
        assert_eq!(
            files_fetch_error(uuid::Uuid::nil(), reqwest::StatusCode::NOT_FOUND).status,
            404
        );
        for status in [401, 403, 500, 503] {
            let err = files_fetch_error(
                uuid::Uuid::nil(),
                reqwest::StatusCode::from_u16(status).unwrap(),
            );
            assert_eq!(err.status, 502, "upstream {status} must not report 404");
        }
    }

    fn mirror_backend() -> (
        tempfile::TempDir,
        std::sync::Arc<dyn kallip_blob_store::BlobStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(dir.path().to_owned());
        (dir, backend)
    }

    fn some_record() -> uuid::Uuid {
        uuid::Uuid::from_u128(42)
    }

    #[tokio::test]
    async fn local_hit_serves_bytes_without_touching_files() {
        let (_dir, backend) = mirror_backend();
        let id = store_mirror(Some(&backend), &[1, 2, 3]).await.unwrap();
        // The files future panics if it is ever awaited: a local hit must
        // never reach it.
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async { panic!("files fetch must not run for a local hit") },
            Some(&id),
        )
        .await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![1, 2, 3]);
            }
            other => panic!("expected a local hit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_copy_backfills_from_files() {
        let (_dir, backend) = mirror_backend();
        let anchor = kallip_blob_store::BlobId::for_bytes(&[9, 9])
            .as_str()
            .to_owned();
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async { Ok(vec![9, 9]) },
            Some(&anchor),
        )
        .await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![9, 9]);
            }
            other => panic!("expected fetched bytes, got {other:?}"),
        }
        // The fetched bytes were written back into the mirror.
        let stored = backend
            .get(&kallip_blob_store::BlobId::parse(&anchor).unwrap())
            .await
            .unwrap();
        assert_eq!(stored, vec![9, 9]);
    }

    #[tokio::test]
    async fn gone_with_a_local_copy_is_kept() {
        let (_dir, backend) = mirror_backend();
        let id = store_mirror(Some(&backend), &[4, 5]).await.unwrap();
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async {
                Err(files_fetch_error(
                    uuid::Uuid::nil(),
                    reqwest::StatusCode::NOT_FOUND,
                ))
            },
            Some(&id),
        )
        .await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![4, 5]);
            }
            other => panic!("the local copy must win over a files 404, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gone_without_the_copy_is_deterministic() {
        let (_dir, backend) = mirror_backend();
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async {
                Err(files_fetch_error(
                    uuid::Uuid::nil(),
                    reqwest::StatusCode::NOT_FOUND,
                ))
            },
            None,
        )
        .await;
        assert!(matches!(
            verdict,
            kallip_runtime::context::FetchedImage::Gone
        ));
    }

    #[tokio::test]
    async fn transient_failure_stays_transient() {
        let (_dir, backend) = mirror_backend();
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async {
                Err(files_fetch_error(
                    uuid::Uuid::nil(),
                    reqwest::StatusCode::BAD_GATEWAY,
                ))
            },
            None,
        )
        .await;
        assert!(matches!(
            verdict,
            kallip_runtime::context::FetchedImage::Transient(_)
        ));
    }

    #[tokio::test]
    async fn absent_anchor_goes_to_files_and_backfills() {
        let (_dir, backend) = mirror_backend();
        // No blob id (a pre-mirror record): straight to files, and the
        // fetched bytes still land in the mirror.
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async { Ok(vec![6, 6]) },
            None,
        )
        .await;
        assert!(matches!(
            verdict,
            kallip_runtime::context::FetchedImage::Bytes(_)
        ));
        let anchor = kallip_blob_store::BlobId::for_bytes(&[6, 6]);
        assert_eq!(
            backend.stat(&anchor).await.unwrap().map(|b| b.size),
            Some(2)
        );
    }

    #[tokio::test]
    async fn store_mirror_without_a_store_is_none() {
        assert_eq!(store_mirror(None, &[1]).await, None);
    }

    #[tokio::test]
    async fn store_mirror_write_failure_falls_back_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(blocker);
        assert_eq!(store_mirror(Some(&backend), &[1]).await, None);
    }

    #[tokio::test]
    async fn malformed_anchor_stays_files_backed() {
        let (_dir, backend) = mirror_backend();
        // A blob id that fails validation drops the local arm entirely:
        // the reference stays files-backed instead of failing the fetch.
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async { Ok(vec![7, 7]) },
            Some("sha256-not-hex"),
        )
        .await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![7, 7]);
            }
            other => panic!("expected files-backed bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn absent_store_reads_files_directly() {
        // No store installed (the store-less startup window): even a
        // well-formed anchor is unusable and the read goes to files.
        let anchor = format!("sha256-{}", "a".repeat(64));
        let verdict =
            fetch_local_first(None, some_record(), async { Ok(vec![8, 8]) }, Some(&anchor)).await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![8, 8]);
            }
            other => panic!("expected files-backed bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn backfill_failure_still_serves_bytes() {
        // The mirror root is a regular file, so the backfill put fails;
        // the fetched bytes must still reach the caller (fail-open).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(blocker);
        let verdict = fetch_local_first(
            Some(&backend),
            some_record(),
            async { Ok(vec![3, 1]) },
            None,
        )
        .await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![3, 1]);
            }
            other => panic!("expected served bytes despite backfill failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nil_record_blob_miss_is_gone_without_a_files_call() {
        let (_dir, backend) = mirror_backend();
        let anchor = kallip_blob_store::BlobId::for_bytes(&[1, 1, 1])
            .as_str()
            .to_owned();
        let verdict = fetch_local_first(
            Some(&backend),
            uuid::Uuid::nil(),
            async { panic!("a path-form reference must not reach files") },
            Some(&anchor),
        )
        .await;
        assert!(matches!(
            verdict,
            kallip_runtime::context::FetchedImage::Gone
        ));
    }

    #[tokio::test]
    async fn nil_record_without_a_blob_id_is_gone_without_a_files_call() {
        let (_dir, backend) = mirror_backend();
        let verdict = fetch_local_first(
            Some(&backend),
            uuid::Uuid::nil(),
            async { panic!("a path-form reference must not reach files") },
            None,
        )
        .await;
        assert!(matches!(
            verdict,
            kallip_runtime::context::FetchedImage::Gone
        ));
    }

    #[tokio::test]
    async fn nil_record_with_a_local_hit_still_serves_bytes() {
        let (_dir, backend) = mirror_backend();
        let id = store_mirror(Some(&backend), &[8, 8]).await.unwrap();
        let verdict = fetch_local_first(
            Some(&backend),
            uuid::Uuid::nil(),
            async { panic!("a local hit must not reach files") },
            Some(&id),
        )
        .await;
        match verdict {
            kallip_runtime::context::FetchedImage::Bytes(bytes) => {
                assert_eq!(bytes, vec![8, 8]);
            }
            other => panic!("expected the local copy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_without_a_registered_credential_is_unavailable() {
        // The credential check precedes the URL read, so the credential
        // error is independent of the environment. The async env lock is
        // held across the awaits below: both error paths read the
        // variable, so the unset window must cover the whole body.
        kallip_testkit::with_env_async(&[("KALLIP_POLIS_URL", None)], async {
            let err = fetch_record_bytes(&reqwest::Client::new(), None, uuid::Uuid::nil())
                .await
                .unwrap_err();
            assert_eq!(err.status, 503);
            assert!(err.message.contains("no registered files credential"));
            // A registered token still takes the URL from the environment
            // (non-secret process configuration).
            let err = fetch_record_bytes(&reqwest::Client::new(), Some("tok"), uuid::Uuid::nil())
                .await
                .unwrap_err();
            assert_eq!(err.status, 503);
            assert!(err.message.contains("KALLIP_POLIS_URL is not set"));
        })
        .await;
    }
}
