//! One outbound request shape for a platform credential: send, and send a
//! second time with the reloaded value when the platform rejects the one we
//! presented. The tagma's readers (the files fetch, the gateway GET and the
//! gateway selection PUT) differ only in how they build the request, so the
//! retry control flow lives here once.

use kallipai_common::credential::{Credential, retry_value_after_401};
use reqwest::Response;
use reqwest_middleware::RequestBuilder;

/// The response plus the credential value the request that produced it
/// carried: a `401` retried with the reloaded value answers with that value,
/// so a caller recording a rejection records what the platform saw rather
/// than whatever the handle holds a few moments later.
pub(crate) struct Presented {
    pub(crate) response: Response,
    pub(crate) credential: String,
}

/// Send `build` once, and once more when the answer is `401` and the store
/// behind the handle holds a different value.
///
/// `build` runs again for the second attempt, so the request is rebuilt rather
/// than replayed; a caller with per-call material (a URL carrying the record
/// id, a JSON body) needs no special case. Only the response head is
/// inspected — the body stays unconsumed — so the caller still decides what a
/// remaining `401` means.
///
/// `build` hands back the client's middleware request builder, so a retry takes
/// the same chain as the first attempt, and a failure comes back as the
/// middleware's error: the caller formats it and keeps its own error type.
pub(crate) async fn send_authorized(
    credential: &Credential,
    surface: &str,
    build: impl Fn(&str) -> RequestBuilder,
) -> Result<Presented, reqwest_middleware::Error> {
    let sent = credential.current();
    let response = build(&sent).send().await?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(Presented {
            response,
            credential: sent,
        });
    }
    match retry_value_after_401(credential, &sent, surface) {
        Some(reloaded) => Ok(Presented {
            response: build(&reloaded).send().await?,
            credential: reloaded,
        }),
        None => Ok(Presented {
            response,
            credential: sent,
        }),
    }
}
