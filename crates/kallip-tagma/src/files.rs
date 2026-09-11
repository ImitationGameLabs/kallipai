//! Files-service access for the tagma process: fetching stored media with
//! the tagma's own credentials. Shared by the attachment-ingest route
//! (live reads) and the restore path (re-assembly after a restart).

use kallip_common::protocol::ApiError;

/// Fetch the record's bytes from the files service (the tagma process's
/// own credentials). Whole-body: the service caps uploads, so a stream
/// would add plumbing without changing the memory story.
pub(crate) async fn fetch_record_bytes(
    http: &reqwest::Client,
    record_id: uuid::Uuid,
) -> Result<Vec<u8>, ApiError> {
    let base = std::env::var("KALLIP_FILES_URL").map_err(|_| {
        ApiError::unavailable("KALLIP_FILES_URL is not set; the tagma cannot fetch media")
    })?;
    let token = std::env::var("KALLIP_FILES_TOKEN").map_err(|_| {
        ApiError::unavailable("KALLIP_FILES_TOKEN is not set; the tagma cannot fetch media")
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

/// The re-assembly fetch verdict for one record: `Gone` only for a
/// missing record (deterministic — the reference is invalidated once and
/// skipped from then on); everything else (credentials, 5xx, network) is
/// transient, so the restore stays text-only for that reference and the
/// next boot tries again.
pub(crate) async fn fetch_for_reassembly(
    http: &reqwest::Client,
    record_id: uuid::Uuid,
) -> kallip_runtime::context::FetchedImage {
    match fetch_record_bytes(http, record_id).await {
        Ok(bytes) => kallip_runtime::context::FetchedImage::Bytes(bytes),
        Err(e) if e.status == 404 => kallip_runtime::context::FetchedImage::Gone,
        Err(e) => kallip_runtime::context::FetchedImage::Transient(e.to_string()),
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
}
