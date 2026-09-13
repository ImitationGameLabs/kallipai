//! GET / HEAD /{id}: authorize on the record's space path, then
//! stream the blob through windowed `get_range` calls. Large blobs are
//! never read whole: the response body is a sequence of fixed-size windows,
//! so peak memory is one window regardless of file size.

use std::convert::Infallible;
use std::pin::Pin;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use uuid::Uuid;

use super::{load_record, parse_record_path};
use crate::auth::AuthPrincipal;
use crate::state::AppState;
use kallip_blob_store::BlobId;
use kallip_common::protocol::ApiError;

/// Window size for streamed reads: the service reads the blob this many
/// bytes at a time, and the response body is built from these windows.
const STREAM_WINDOW: u64 = 128 * 1024;

/// GET /{id}, honoring a single-range `Range` header.
pub async fn get_file(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Response, ApiError> {
    let record = load_record(&state, id).await?;
    let path = parse_record_path(&record.space_path)?;
    super::authorize(&state, &principal, &path, crate::acl::Action::Read).await?;
    let blob_id = parse_blob_id(&record.blob_id)?;
    let size = stat_size(&state, &blob_id).await?;

    match parse_range(range_header(&headers), size) {
        RangeOutcome::Unsatisfiable => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            [("content-range", format!("bytes */{size}"))],
        )
            .into_response()),
        RangeOutcome::Full => Ok(stream_body(&state, blob_id, 0, size, false, size)),
        RangeOutcome::Slice { start, end } => Ok(stream_body(
            &state,
            blob_id,
            start,
            end - start + 1,
            true,
            size,
        )),
    }
}

/// HEAD /{id}: the same authorization and metadata as GET, no
/// body.
pub async fn head_file(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Response, ApiError> {
    let record = load_record(&state, id).await?;
    let path = parse_record_path(&record.space_path)?;
    super::authorize(&state, &principal, &path, crate::acl::Action::Read).await?;
    let blob_id = parse_blob_id(&record.blob_id)?;
    let size = stat_size(&state, &blob_id).await?;
    Ok((
        StatusCode::OK,
        [
            ("content-length", size.to_string()),
            ("accept-ranges", "bytes".to_owned()),
            ("content-type", "application/octet-stream".to_owned()),
        ],
    )
        .into_response())
}

/// Validate the blob id stored in the catalog row. Ids are checked at
/// ingest time, so a malformed value here means catalog corruption, not
/// a client mistake -- hence 500 rather than 400. The store's
/// invalid-id/missing-blob distinction never surfaces as 4xx on this
/// path: a valid id whose file is gone is the reconciler's drift (see
/// `stat_size`); only an endpoint accepting client-supplied id strings
/// would map malformed input to 400.
fn parse_blob_id(raw: &str) -> Result<BlobId, ApiError> {
    BlobId::parse(raw).map_err(ApiError::internal)
}

/// Stat the blob behind a record. `None` here is the reconciler's
/// "missing_blobs" drift (a catalog row whose file is gone): a
/// user-visible data loss, so a 500 -- never a 404, which would claim the
/// record does not exist.
async fn stat_size(state: &AppState, blob_id: &BlobId) -> Result<u64, ApiError> {
    let info = state.blob.stat(blob_id).await.map_err(ApiError::internal)?;
    info.map(|info| info.size)
        .ok_or_else(|| ApiError::internal("catalog row without a blob file"))
}

fn range_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(axum::http::header::RANGE)?.to_str().ok()
}

/// The outcome of parsing a `Range` header against a known size.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RangeOutcome {
    /// Serve the whole blob: no header, a malformed value, or an
    /// unsupported form (multi-range) -- the RFC lets an unsupported range
    /// be ignored.
    Full,
    /// Serve `[start, end]` inclusive as a 206.
    Slice { start: u64, end: u64 },
    /// A syntactically valid single range that cannot be served: 416.
    Unsatisfiable,
}

/// Parse a single-range `bytes=` header. Multi-range forms are unsupported
/// and ignored (`Full`); malformed values are ignored too; a
/// valid but unservable range (start at/after EOF, zero-length suffix, an
/// inverted span) is `Unsatisfiable`.
pub(crate) fn parse_range(header: Option<&str>, size: u64) -> RangeOutcome {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return RangeOutcome::Full;
    };
    if spec.contains(',') {
        return RangeOutcome::Full;
    }
    let spec = spec.trim();
    if let Some(suffix) = spec.strip_prefix('-') {
        // bytes=-N: the last N bytes.
        let Ok(n) = suffix.parse::<u64>() else {
            return RangeOutcome::Full;
        };
        if n == 0 || size == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        let start = size.saturating_sub(n);
        return RangeOutcome::Slice {
            start,
            end: size - 1,
        };
    }
    let Some((start_raw, end_raw)) = spec.split_once('-') else {
        return RangeOutcome::Full;
    };
    let (Ok(start), end_raw) = (start_raw.parse::<u64>(), end_raw.trim()) else {
        return RangeOutcome::Full;
    };
    if start >= size {
        return RangeOutcome::Unsatisfiable;
    }
    if end_raw.is_empty() {
        // bytes=S-: open-ended to EOF.
        return RangeOutcome::Slice {
            start,
            end: size - 1,
        };
    }
    let Ok(end) = end_raw.parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if end < start {
        return RangeOutcome::Unsatisfiable;
    }
    RangeOutcome::Slice {
        start,
        end: end.min(size - 1),
    }
}

/// Build the streamed response: `len` bytes from `start`, delivered in
/// [`STREAM_WINDOW`] windows straight off the store. `partial` selects 206
/// (with `Content-Range`) over 200.
fn stream_body(
    state: &AppState,
    blob_id: BlobId,
    start: u64,
    len: u64,
    partial: bool,
    total: u64,
) -> Response {
    let store = state.blob.clone();
    let stream: Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>> =
        Box::pin(futures_util::stream::unfold(
            (store, blob_id, start, len),
            |(store, blob_id, offset, remaining)| async move {
                if remaining == 0 {
                    return None;
                }
                let take = remaining.min(STREAM_WINDOW);
                match store.get_range(&blob_id, offset, take).await {
                    Ok(bytes) => {
                        let out = Result::<Bytes, Infallible>::Ok(Bytes::from(bytes));
                        Some((out, (store, blob_id, offset + take, remaining - take)))
                    }
                    Err(e) => {
                        // A mid-stream store failure cannot change the
                        // status anymore; the short body breaks the
                        // content-length contract and with it the connection;
                        // log the drift for the reconciler.
                        tracing::error!(error = %e, "blob read failed mid-stream");
                        Some((
                            Result::<Bytes, Infallible>::Ok(Bytes::new()),
                            (store, blob_id, offset, 0),
                        ))
                    }
                }
            },
        ));
    let mut headers = vec![
        ("accept-ranges".to_owned(), "bytes".to_owned()),
        (
            "content-type".to_owned(),
            "application/octet-stream".to_owned(),
        ),
        ("content-length".to_owned(), len.to_string()),
    ];
    if partial {
        headers.push((
            "content-range".to_owned(),
            format!("bytes {start}-{end}/{total}", end = start + len - 1),
        ));
    }
    let mut response = (StatusCode::OK, axum::body::Body::from_stream(stream)).into_response();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            name.parse::<axum::http::HeaderName>(),
            value.parse::<axum::http::HeaderValue>(),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    if partial {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    response
}
