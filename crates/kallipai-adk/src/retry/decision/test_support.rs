//! Builders for the errors the decision tests feed the classifier: a provider
//! rejection with a status and body, one with a typed source, a `serde_json`
//! failure, and the classifier call over them. Two facets compare rows of the
//! class table (`classify`) and of the fact table (`blocks`); both start from
//! the same errors.

use std::time::Duration;

use just_llm_client::{BackendError, ProviderError, TransportError};
use reqwest::StatusCode;

use super::attempt::Phase;
use super::classify::classify;
use super::failure_class::FailureClass;
use crate::retry::RetryPolicy;

/// A rejection that reached the provider, in the shape the client produces:
/// the status rides a `TransportError` inside the boxed source chain.
pub(super) fn provider_status(status: StatusCode, body: &str) -> BackendError {
    BackendError::provider(
        "openai-compatible",
        ProviderError::Transport(TransportError::HttpStatus {
            status,
            body: body.to_string(),
        }),
    )
}

/// A provider error whose source is anything else: the typed rows of the table
/// are reached by walking the chain, whatever the concrete type is.
pub(super) fn provider_source<E>(source: E) -> BackendError
where
    E: std::error::Error + Send + Sync + 'static,
{
    BackendError::provider("openai-compatible", source)
}

/// A `serde_json` failure of the category the text produces: `Data` (wrong
/// shape), `Eof` (truncated), `Syntax` (malformed).
pub(super) fn json_error(text: &str) -> serde_json::Error {
    serde_json::from_str::<u32>(text).expect_err("not a number")
}

/// The table's connect-phase reading, the phase most cases exercise; the
/// mid-stream row has its own case below.
pub(super) fn classify_connect(error: &BackendError) -> FailureClass {
    classify(error, Phase::Connect)
}

pub(super) fn policy(max_retries: u32, base: u64, max: u64, timeout: u64) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::from_secs(base),
        max_delay: Duration::from_secs(max),
        retry_timeout: Duration::from_secs(timeout),
    }
}
