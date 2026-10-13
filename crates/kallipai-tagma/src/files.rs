//! Files-service access for the tagma process: fetching stored media with
//! the tagma's own credentials. The record-form ingest
//! (`POST /agents/{id}/attachments/ingest`) reaches the platform a record
//! lives on; the restore path (re-assembly after a restart) reads the
//! tagma's own attachment store instead.

use kallipai_common::protocol::ApiError;

/// One platform the tagma can fetch records from: the relay entry's name
/// (the error's attribution), that entry's origin, and the enrollment
/// token stored for it. The origin and the bearer always come from the
/// same entry: the token is that platform's credential, so an origin is
/// never paired with a foreign credential.
pub(crate) struct FilesAccess {
    pub(crate) name: String,
    pub(crate) origin: String,
    pub(crate) token: String,
}

/// Fetch the record's bytes from the files service under the tagma's own
/// registered credentials. A record lives on exactly one platform, so the
/// walk visits the enrolled platforms in configuration order: a 404 means
/// not on this one: ask the next; any other failure stops the walk, and
/// the error names the platform it came from. A record no enrolled
/// platform has is reported with the faces that were asked.
/// Whole-body: the service caps uploads, so a stream would add plumbing
/// without changing the memory story.
pub(crate) async fn fetch_record_bytes(
    http: &reqwest::Client,
    accesses: &[FilesAccess],
    record_id: uuid::Uuid,
) -> Result<Vec<u8>, ApiError> {
    if accesses.is_empty() {
        return Err(ApiError::unavailable(
            "the tagma is enrolled on no platform; it cannot fetch media",
        ));
    }
    let mut asked = Vec::with_capacity(accesses.len());
    for access in accesses {
        asked.push(access.name.as_str());
        if let Some(bytes) = fetch_from_face(http, access, record_id).await? {
            return Ok(bytes);
        }
    }
    Err(absent_everywhere(record_id, &asked))
}

/// One platform's attempt: its own origin, its own bearer; 404 is a miss.
async fn fetch_from_face(
    http: &reqwest::Client,
    access: &FilesAccess,
    record_id: uuid::Uuid,
) -> Result<Option<Vec<u8>>, ApiError> {
    let response = http
        .get(format!("{}/v1/files/{record_id}", access.origin))
        .header("authorization", format!("Bearer {}", access.token))
        .send()
        .await
        .map_err(|e| {
            ApiError::unavailable(format!("files fetch failed via {}: {e}", access.name))
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(files_upstream_error(record_id, status, &access.name));
    }
    response
        .bytes()
        .await
        .map(|b| Some(b.to_vec()))
        .map_err(|e| ApiError::unavailable(format!("files read failed via {}: {e}", access.name)))
}

/// The 404 for a walk that came up empty: the conclusion covers every
/// face that was asked, so it names them.
fn absent_everywhere(record_id: uuid::Uuid, asked: &[&str]) -> ApiError {
    ApiError::not_found(format!(
        "files record {record_id} does not exist on any enrolled platform; asked {}",
        asked.join(", ")
    ))
}

/// Map a failure status to an upstream fault (credentials, service
/// health) surfaced as a gateway error that names the platform and
/// keeps the upstream status for triage.
fn files_upstream_error(
    record_id: uuid::Uuid,
    status: reqwest::StatusCode,
    platform: &str,
) -> ApiError {
    ApiError::bad_gateway(format!(
        "files fetch for {record_id} via {platform}: upstream HTTP {status}"
    ))
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
    fn files_upstream_error_folds_every_status_into_bad_gateway() {
        for status in [401, 403, 404, 500, 503] {
            let err = files_upstream_error(
                uuid::Uuid::nil(),
                reqwest::StatusCode::from_u16(status).unwrap(),
                "alpha",
            );
            assert_eq!(err.status, 502, "upstream {status} is a gateway fault");
            assert!(err.message.contains("via alpha"), "got: {}", err.message);
        }
    }

    #[test]
    fn an_empty_walk_names_the_faces_it_asked() {
        let err = absent_everywhere(uuid::Uuid::nil(), &["alpha", "beta"]);
        assert_eq!(err.status, 404);
        assert!(
            err.message
                .contains("does not exist on any enrolled platform"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.ends_with("asked alpha, beta"),
            "got: {}",
            err.message
        );
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

    fn access(name: &str, origin: &str, token: &str) -> FilesAccess {
        FilesAccess {
            name: name.to_owned(),
            origin: origin.to_owned(),
            token: token.to_owned(),
        }
    }

    #[tokio::test]
    async fn the_walk_takes_the_first_platform_that_has_the_record() {
        let (first_origin, first_seen) = crate::test_helpers::spawn_files_stub(404, b"").await;
        let (second_origin, second_seen) =
            crate::test_helpers::spawn_files_stub(200, b"the bytes").await;
        let accesses = [
            access("alpha", &first_origin, "alpha-token"),
            access("beta", &second_origin, "beta-token"),
        ];
        let bytes = fetch_record_bytes(&reqwest::Client::new(), &accesses, uuid::Uuid::nil())
            .await
            .unwrap();
        assert_eq!(bytes, b"the bytes".to_vec());
        assert_eq!(
            first_seen.lock().unwrap().len(),
            1,
            "the first platform is asked first"
        );
        let second = second_seen.lock().unwrap().clone();
        assert_eq!(second.len(), 1, "the record's own platform serves it");
        assert_eq!(second[0].0, format!("/v1/files/{}", uuid::Uuid::nil()));
        assert_eq!(
            second[0].1.as_deref(),
            Some("Bearer beta-token"),
            "each platform carries its own bearer"
        );
    }

    #[tokio::test]
    async fn a_record_no_platform_has_is_not_found() {
        let (first_origin, _first_seen) = crate::test_helpers::spawn_files_stub(404, b"").await;
        let (second_origin, second_seen) = crate::test_helpers::spawn_files_stub(404, b"").await;
        let accesses = [
            access("alpha", &first_origin, "a-token"),
            access("beta", &second_origin, "b-token"),
        ];
        let err = fetch_record_bytes(&reqwest::Client::new(), &accesses, uuid::Uuid::nil())
            .await
            .unwrap_err();
        assert_eq!(err.status, 404);
        assert!(
            err.message
                .contains("does not exist on any enrolled platform"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.ends_with("asked alpha, beta"),
            "got: {}",
            err.message
        );
        assert_eq!(
            second_seen.lock().unwrap().len(),
            1,
            "every enrolled platform was asked"
        );
    }

    #[tokio::test]
    async fn an_upstream_fault_stops_the_walk_and_names_the_platform() {
        let (first_origin, _first_seen) = crate::test_helpers::spawn_files_stub(500, b"").await;
        let (second_origin, second_seen) =
            crate::test_helpers::spawn_files_stub(200, b"the bytes").await;
        let accesses = [
            access("alpha", &first_origin, "a-token"),
            access("beta", &second_origin, "b-token"),
        ];
        let err = fetch_record_bytes(&reqwest::Client::new(), &accesses, uuid::Uuid::nil())
            .await
            .unwrap_err();
        assert_eq!(err.status, 502);
        assert!(err.message.contains("via alpha"), "got: {}", err.message);
        assert!(
            second_seen.lock().unwrap().is_empty(),
            "an upstream fault is not answered by another platform"
        );
    }

    #[tokio::test]
    async fn an_unreachable_platform_names_itself_and_stops_the_walk() {
        let (second_origin, second_seen) =
            crate::test_helpers::spawn_files_stub(200, b"the bytes").await;
        let accesses = [
            access("alpha", "http://127.0.0.1:1", "a-token"),
            access("beta", &second_origin, "b-token"),
        ];
        let err = fetch_record_bytes(&reqwest::Client::new(), &accesses, uuid::Uuid::nil())
            .await
            .unwrap_err();
        assert_eq!(err.status, 503);
        assert!(
            err.message.contains("files fetch failed via alpha"),
            "got: {}",
            err.message
        );
        assert!(second_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_enrolled_platform_is_unavailable() {
        let err = fetch_record_bytes(&reqwest::Client::new(), &[], uuid::Uuid::nil())
            .await
            .unwrap_err();
        assert_eq!(err.status, 503);
        assert!(
            err.message.contains("enrolled on no platform"),
            "got: {}",
            err.message
        );
    }
}
