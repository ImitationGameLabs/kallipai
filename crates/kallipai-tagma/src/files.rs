//! Files-service access for the tagma process: fetching stored media with
//! the tagma's own credentials. Shared by the attachment-ingest route
//! (live reads) and the restore path (re-assembly after a restart).

use kallipai_common::protocol::ApiError;

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
    let base = std::env::var("KALLIPAI_POLIS_URL").map_err(|_| {
        ApiError::unavailable("KALLIPAI_POLIS_URL is not set; the tagma cannot fetch media")
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
/// The re-assembly fetch for one reference: a pure local read of the
/// master copy from the tagma's attachment store. A missing blob — or an
/// anchor that does not parse (a legacy line's serde-default empty
/// anchor among them) — is deterministic `Gone`; any other local-read
/// failure is transient on its own. There is no network fallback:
/// the local store is the only byte source, and an uninitialized
/// store fails closed (every image Gone, nothing silently
/// reaches the network).
pub(crate) async fn fetch_local(
    blobs: Option<&std::sync::Arc<dyn kallipai_blob_store::BlobStore>>,
    blob_id: &str,
) -> kallipai_adk::context::FetchedImage {
    use kallipai_adk::context::FetchedImage;
    let Some(blobs) = blobs else {
        tracing::warn!("attachment blob store is not configured; treating every image as gone");
        return FetchedImage::Gone;
    };
    let Ok(id) = kallipai_blob_store::BlobId::parse(blob_id) else {
        // A damaged anchor classifies as Gone, never Transient: a
        // damaged line must not enter the retry loop.
        return FetchedImage::Gone;
    };
    match blobs.get(&id).await {
        Ok(bytes) => FetchedImage::Bytes(bytes),
        Err(kallipai_blob_store::Error::NotFound(_)) => FetchedImage::Gone,
        Err(e) => {
            tracing::warn!("attachment blob read failed: {e}");
            FetchedImage::Transient(e.to_string())
        }
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

    fn blob_backend() -> (
        tempfile::TempDir,
        std::sync::Arc<dyn kallipai_blob_store::BlobStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        (dir, backend)
    }

    async fn put(
        backend: &std::sync::Arc<dyn kallipai_blob_store::BlobStore>,
        bytes: &[u8],
    ) -> String {
        backend
            .put(&mut std::io::Cursor::new(bytes))
            .await
            .unwrap()
            .id
            .as_str()
            .to_owned()
    }

    #[tokio::test]
    async fn fetch_local_serves_the_stored_master_copy() {
        let (_dir, backend) = blob_backend();
        let anchor = put(&backend, &[1, 2, 3]).await;
        match fetch_local(Some(&backend), &anchor).await {
            kallipai_adk::context::FetchedImage::Bytes(bytes) => assert_eq!(bytes, vec![1, 2, 3]),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_blob_is_deterministically_gone() {
        let (_dir, backend) = blob_backend();
        let anchor = put(&backend, &[1]).await;
        let missing = format!("sha256-{}", "b".repeat(64));
        assert_ne!(
            missing, anchor,
            "the missing anchor must differ from the stored one"
        );
        assert!(matches!(
            fetch_local(Some(&backend), &missing).await,
            kallipai_adk::context::FetchedImage::Gone
        ));
    }

    #[tokio::test]
    async fn a_non_parsing_anchor_is_gone_never_transient() {
        let (_dir, backend) = blob_backend();
        assert!(matches!(
            fetch_local(Some(&backend), "").await,
            kallipai_adk::context::FetchedImage::Gone
        ));
    }

    #[tokio::test]
    async fn an_uninitialized_store_fails_closed_to_gone() {
        assert!(matches!(
            fetch_local(None, "sha256-deadbeef").await,
            kallipai_adk::context::FetchedImage::Gone
        ));
    }

    #[tokio::test]
    async fn a_local_read_failure_other_than_missing_is_transient() {
        // A file where the store root should be: every read fails with an
        // IO error, which must classify as Transient (retryable), not Gone.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::write(&root, b"not a directory").unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(root);
        assert!(matches!(
            fetch_local(Some(&backend), &format!("sha256-{}", "c".repeat(64))).await,
            kallipai_adk::context::FetchedImage::Transient(_)
        ));
    }

    #[tokio::test]
    async fn record_fetch_without_enrollment_is_unavailable() {
        // The credential check precedes the URL read, so the credential
        // error is independent of the environment. The async env lock is
        // held across the awaits below: both error paths read the
        // variable, so the unset window must cover the whole body.
        kallipai_testkit::with_env_async(&[("KALLIPAI_POLIS_URL", None)], async {
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
            assert!(err.message.contains("KALLIPAI_POLIS_URL is not set"));
        })
        .await;
    }
}
