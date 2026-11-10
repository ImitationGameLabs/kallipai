//! Reading one typed error into the vocabulary: HTTP status, provider error
//! code, transport variant, `serde_json` category — plus the evidence only body
//! text carries (quota strings, and the moment a quota wall reopens).
//!
//! The table this file consults is the class table defined in
//! [`failure_class`](super::failure_class).

use just_llm_client::{
    BackendError, ProviderError, TransportError, captured_body, provider_rejection,
};

use super::attempt::Phase;
use super::failure_class::{FailureClass, ShapeKey, TransientKind};

/// Evidence substrings that make a 403 a quota rejection.
///
/// A 403 is ambiguous: it covers both "this key may not do that" (endpoint-level)
/// and "this account is out of credit" (quota-level). The provider's own words are
/// the only signal available here, so the test is a case-insensitive substring
/// match over the error code, the error type, and the captured body.
const QUOTA_EVIDENCE: [&str; 4] = ["insufficient_quota", "quota", "billing", "x-ratelimit"];

/// Classify a backend error. The result feeds the exit mapping; it carries no
/// verdict of its own.
///
/// Order, fixed: the top-level variant, then the HTTP status the provider
/// answered with, then a walk down the `source()` chain. The first node that can
/// be classified wins and the walk stops, so the same error cannot classify
/// differently depending on how deeply it is nested. A chain that yields nothing
/// classifies as [`FailureClass::Unclassified`] — the conservative reading, no
/// retry.
///
/// One deliberate consequence: a top-level `Provider` that wraps a shape-level
/// failure inside a 5xx classifies as `Transient(Server)` and takes the backoff
/// path rather than a shape path. The classifier sees typed values only and does
/// not guess at body semantics.
///
/// `phase` changes one row: a 400/422 is the request itself before content flows
/// and a shape problem after it (see [`from_status`]).
pub(crate) fn classify(error: &BackendError, phase: Phase) -> FailureClass {
    match error {
        // Pre-flight failures: the request never reached the provider, and it
        // fails identically on every profile.
        BackendError::InvalidRequest(_)
        | BackendError::Serialization { .. }
        | BackendError::Unserializable { .. } => return FailureClass::RequestPermanent,
        BackendError::Provider { .. } => {}
        // `BackendError` is `#[non_exhaustive]`.
        _ => return FailureClass::Unclassified,
    }

    // A provider response carries its status through the boxed source chain;
    // `provider_rejection` is the upstream accessor for that walk. `None` means
    // the error never reached a provider response.
    if let Some(rejection) = provider_rejection(error) {
        return from_status(
            rejection.status,
            rejection.code.as_deref(),
            rejection.error_type.as_deref(),
            captured_body(error),
            phase,
        );
    }

    let mut node = std::error::Error::source(error);
    while let Some(source) = node {
        if let Some(provider) = source.downcast_ref::<ProviderError>() {
            return match provider {
                ProviderError::Serialize(_) | ProviderError::InvalidRequest(_) => {
                    FailureClass::RequestPermanent
                }
                ProviderError::Deserialize { source, .. } => from_json_category(source),
                ProviderError::Transport(transport) => from_transport(transport, phase),
                // `ProviderError` is `#[non_exhaustive]`.
                _ => FailureClass::Unclassified,
            };
        }
        if let Some(transport) = source.downcast_ref::<TransportError>() {
            return from_transport(transport, phase);
        }
        node = source.source();
    }
    FailureClass::Unclassified
}

/// The shape key of a failure the table classified as [`FailureClass::ResponseShape`],
/// or `None` when the error carries no shape-class leaf.
///
/// The same walk as [`classify`]: the first shape-producing node on the source
/// chain wins, so the key of a nested failure does not depend on its depth.
pub(crate) fn shape_key(error: &BackendError) -> Option<ShapeKey> {
    let mut node = std::error::Error::source(error);
    while let Some(source) = node {
        if let Some(provider) = source.downcast_ref::<ProviderError>()
            && let ProviderError::Deserialize { source, .. } = provider
        {
            return match source.classify() {
                serde_json::error::Category::Data => Some(ShapeKey::BodyStructure),
                // Truncation and I/O are read problems, not shapes.
                _ => None,
            };
        }
        if let Some(transport) = source.downcast_ref::<TransportError>() {
            return match transport {
                TransportError::Deserialize { source, .. } => match source.classify() {
                    serde_json::error::Category::Data => Some(ShapeKey::FrameStructure),
                    _ => None,
                },
                TransportError::Utf8(_) => Some(ShapeKey::NotUtf8),
                TransportError::BodyTooLarge { .. } => Some(ShapeKey::OversizedBody),
                TransportError::InvalidResponse(_) => Some(ShapeKey::MalformedResponse),
                // The statuses the table reads as a shape once content has flowed:
                // the same status twice is the repeat the gate catches.
                TransportError::HttpStatus { status, .. } => match *status {
                    reqwest::StatusCode::BAD_REQUEST
                    | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                        Some(ShapeKey::StatusRejected(*status))
                    }
                    _ => None,
                },
                _ => None,
            };
        }
        node = source.source();
    }
    None
}

/// The HTTP-status row of the table. `code`, `error_type`, and `body` are the
/// provider's own words, consulted only to disambiguate a 403.
///
/// A 400/422 is request-level before content streams — resending the same
/// request cannot help — and shape-level after it, where the late form is what
/// the episode's one shape retry covers.
fn from_status(
    status: reqwest::StatusCode,
    code: Option<&str>,
    error_type: Option<&str>,
    body: Option<&str>,
    phase: Phase,
) -> FailureClass {
    use reqwest::StatusCode as Status;
    match status {
        // A 429 that names its reset moment is a quota rejection like any
        // other: 402 and a 403 carrying quota evidence join it. One that
        // names none is a throttle — the upstream is still saying later, not
        // that this account is out — so the episode spends one jittered send
        // on it before the chain is charged.
        Status::TOO_MANY_REQUESTS if !body_carries_reset_hint(body) => FailureClass::Throttled,
        Status::TOO_MANY_REQUESTS | Status::PAYMENT_REQUIRED => FailureClass::QuotaBlocked,
        Status::FORBIDDEN if quota_evidence(code, error_type, body) => FailureClass::QuotaBlocked,
        // Credentials or endpoint identity: another profile may serve.
        Status::FORBIDDEN | Status::UNAUTHORIZED | Status::NOT_FOUND => {
            FailureClass::EndpointPermanent
        }
        // The request itself, or the shape the response took: the same status
        // reads differently depending on whether content had flowed.
        Status::BAD_REQUEST | Status::UNPROCESSABLE_ENTITY => match phase {
            Phase::Connect => FailureClass::RequestPermanent,
            Phase::MidStream => FailureClass::ResponseShape,
        },
        Status::REQUEST_TIMEOUT => FailureClass::Transient(TransientKind::Timeout),
        status if status.is_server_error() => FailureClass::Transient(TransientKind::Server),
        // Remaining 4xx: the conservative reading — do not retry.
        _ => FailureClass::RequestPermanent,
    }
}

/// Does the provider's own text say this rejection is about quota or payment?
fn quota_evidence(code: Option<&str>, error_type: Option<&str>, body: Option<&str>) -> bool {
    let haystack = [
        code.unwrap_or_default(),
        error_type.unwrap_or_default(),
        body.unwrap_or_default(),
    ]
    .join(" ")
    .to_lowercase();
    QUOTA_EVIDENCE
        .iter()
        .any(|needle| haystack.contains(needle))
}

/// A `serde_json` failure: a truncated body or frame is a transient read problem,
/// while a well-formed body of the wrong shape is a response-shape problem.
fn from_json_category(error: &serde_json::Error) -> FailureClass {
    match error.classify() {
        serde_json::error::Category::Data => FailureClass::ResponseShape,
        serde_json::error::Category::Eof | serde_json::error::Category::Syntax => {
            FailureClass::Transient(TransientKind::Truncated)
        }
        serde_json::error::Category::Io => FailureClass::Transient(TransientKind::Transport),
    }
}

/// The transport-level row of the table.
fn from_transport(error: &TransportError, phase: Phase) -> FailureClass {
    match error {
        // Local configuration or client construction: no request was sent.
        TransportError::InvalidConfig(_) | TransportError::BuildClient(_) => {
            FailureClass::LocalConfig
        }
        // Unreachable from [`classify`], which consults `provider_rejection`
        // first and returns as soon as a status is reachable. Kept total anyway.
        TransportError::HttpStatus { status, body } => {
            from_status(*status, None, None, Some(body), phase)
        }
        TransportError::Transport(_) => FailureClass::Transient(TransientKind::Transport),
        // An SSE frame that failed to parse — a different variant from the
        // whole-body `ProviderError::Deserialize`.
        TransportError::Deserialize { source, .. } => from_json_category(source),
        // Response content problems: wrong encoding, oversized error body, an
        // empty or malformed body.
        TransportError::Utf8(_)
        | TransportError::BodyTooLarge { .. }
        | TransportError::InvalidResponse(_) => FailureClass::ResponseShape,
        // `TransportError` is `#[non_exhaustive]`.
        _ => FailureClass::Unclassified,
    }
}

/// The keys a provider may use for "try again at". Only these three, at any
/// depth: the whitelist is what keeps an unrelated number in a body from
/// reading as a reset hint.
const RESET_HINT_KEYS: [&str; 3] = ["retry_after", "retryDelay", "retry_after_seconds"];

/// Whether the provider named a reset moment: the one hint parser, asked
/// without a clock. An absent, unparsable, or keyless body reads as not
/// given, exactly as `reset_hint` reads it.
fn body_carries_reset_hint(body: Option<&str>) -> bool {
    let Some(body) = body else { return false };
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|value| find_reset_secs(&value).is_some())
}

/// The upstream's reset hint as an absolute moment, when it gave one.
///
/// Read from the captured body alone: the hint arrives as provider-specific
/// JSON, so the body is parsed and the whitelisted keys are searched at any
/// depth. Everything else — no body, a truncated or non-JSON body, a value in a
/// shape the whitelist does not describe — reads as "not given" rather than as a
/// guess.
pub(super) fn reset_hint(error: &BackendError, now: u64) -> Option<u64> {
    let body = captured_body(error)?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let secs = find_reset_secs(&value)?;
    Some(now.saturating_add(secs))
}

/// The first whitelisted key anywhere in the tree, in key order at each level
/// and top level first.
fn find_reset_secs(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => {
            for key in RESET_HINT_KEYS {
                if let Some(secs) = map.get(key).and_then(reset_secs_of) {
                    return Some(secs);
                }
            }
            map.values().find_map(find_reset_secs)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_reset_secs),
        _ => None,
    }
}

/// A whitelisted value: seconds as a number, or as a string with an `s` suffix
/// (`"120s"`). A float, a duration in another unit (`"1m"`), or any other value
/// reads as "not given".
fn reset_secs_of(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text
            .strip_suffix('s')
            .and_then(|secs| secs.trim().parse().ok()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::attempt::Attempt;
    use super::super::blocks::fact_of;
    use super::super::ledger::Episode;
    use super::super::test_support::{
        classify_connect, json_error, policy, provider_source, provider_status,
    };
    use super::super::verdict::{Decision, decide};
    use super::*;
    use just_llm_client::{BackendError, TransportError};
    use reqwest::StatusCode;
    use std::time::Duration;

    /// A 400 is the request itself before content flows; after it, it is the
    /// late form of a shape problem and gets the shape row's one retry.
    #[test]
    fn a_400_reads_by_phase() {
        let error = provider_status(StatusCode::BAD_REQUEST, "");
        assert_eq!(
            classify(&error, Phase::Connect),
            FailureClass::RequestPermanent
        );
        assert_eq!(
            classify(&error, Phase::MidStream),
            FailureClass::ResponseShape
        );
        let unprocessable = provider_status(StatusCode::UNPROCESSABLE_ENTITY, "");
        assert_eq!(
            classify(&unprocessable, Phase::MidStream),
            FailureClass::ResponseShape
        );
    }

    #[test]
    fn preflight_failures_are_request_permanent() {
        assert_eq!(
            classify_connect(&BackendError::invalid_request("unknown model")),
            FailureClass::RequestPermanent
        );
        assert_eq!(
            classify_connect(&BackendError::Serialization {
                source: json_error("nope"),
            }),
            FailureClass::RequestPermanent
        );
        assert_eq!(
            classify_connect(&BackendError::Unserializable {
                family: "openai-compatible",
                message: "no wire form for this request".into(),
            }),
            FailureClass::RequestPermanent
        );
    }

    #[test]
    fn http_status_rows_of_the_table() {
        assert_eq!(
            classify_connect(&provider_status(StatusCode::TOO_MANY_REQUESTS, "")),
            FailureClass::Throttled
        );
        // A 429 that names its reset moment is the quota wall it always was.
        assert_eq!(
            classify_connect(&provider_status(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"retry_after":120}"#
            )),
            FailureClass::QuotaBlocked
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::PAYMENT_REQUIRED, "")),
            FailureClass::QuotaBlocked
        );
        // A 403 is ambiguous; the provider's own words decide.
        assert_eq!(
            classify_connect(&provider_status(
                StatusCode::FORBIDDEN,
                "insufficient_quota"
            )),
            FailureClass::QuotaBlocked
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::FORBIDDEN, "invalid api key")),
            FailureClass::EndpointPermanent
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::UNAUTHORIZED, "")),
            FailureClass::EndpointPermanent
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::NOT_FOUND, "")),
            FailureClass::EndpointPermanent
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::REQUEST_TIMEOUT, "")),
            FailureClass::Transient(TransientKind::Timeout)
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::INTERNAL_SERVER_ERROR, "")),
            FailureClass::Transient(TransientKind::Server)
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::SERVICE_UNAVAILABLE, "")),
            FailureClass::Transient(TransientKind::Server)
        );
        // The remaining 4xx are request-level: resending changes nothing.
        assert_eq!(
            classify_connect(&provider_status(StatusCode::BAD_REQUEST, "")),
            FailureClass::RequestPermanent
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::UNPROCESSABLE_ENTITY, "")),
            FailureClass::RequestPermanent
        );
        // A status no row above names: a server error retries, the rest do not.
        assert_eq!(
            classify_connect(&provider_status(StatusCode::GATEWAY_TIMEOUT, "")),
            FailureClass::Transient(TransientKind::Server)
        );
        assert_eq!(
            classify_connect(&provider_status(StatusCode::METHOD_NOT_ALLOWED, "")),
            FailureClass::RequestPermanent
        );
    }

    #[test]
    fn typed_source_rows_of_the_table() {
        // Client construction failed: no request was sent.
        assert_eq!(
            classify_connect(&provider_source(TransportError::InvalidConfig("no key"))),
            FailureClass::LocalConfig
        );
        // Response content problems are shape problems, not transport ones.
        assert_eq!(
            classify_connect(&provider_source(TransportError::InvalidResponse(
                "empty body".into()
            ))),
            FailureClass::ResponseShape
        );
        assert_eq!(
            classify_connect(&provider_source(TransportError::BodyTooLarge { limit: 64 })),
            FailureClass::ResponseShape
        );
        // A truncated body is transient; a well-formed body of the wrong shape is not.
        assert_eq!(
            classify_connect(&provider_source(ProviderError::Deserialize {
                source: json_error(""),
                body: String::new(),
            })),
            FailureClass::Transient(TransientKind::Truncated)
        );
        assert_eq!(
            classify_connect(&provider_source(ProviderError::Deserialize {
                source: json_error("[]"),
                body: "[]".into(),
            })),
            FailureClass::ResponseShape
        );
        // The same two readings on the frame path: the client lifts an SSE
        // frame that failed to parse into `ProviderError::Transport`.
        assert_eq!(
            classify_connect(&provider_source(TransportError::Deserialize {
                source: json_error(""),
                body: String::new(),
            })),
            FailureClass::Transient(TransientKind::Truncated)
        );
        assert_eq!(
            classify_connect(&provider_source(ProviderError::Transport(
                TransportError::Deserialize {
                    source: json_error("[]"),
                    body: "[]".into(),
                }
            ))),
            FailureClass::ResponseShape
        );
        assert_eq!(
            classify_connect(&provider_source(ProviderError::InvalidRequest(
                "tools unsupported".into()
            ))),
            FailureClass::RequestPermanent
        );
    }

    #[test]
    fn a_chain_with_no_classifiable_node_is_unclassified() {
        assert_eq!(
            classify_connect(&provider_source(std::io::Error::other("boom"))),
            FailureClass::Unclassified
        );
    }

    #[test]
    fn transport_rows_reach_the_same_table() {
        // Unreachable from `classify` (which consults the status first), kept total.
        assert_eq!(
            from_transport(
                &TransportError::HttpStatus {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    body: String::new(),
                },
                Phase::Connect,
            ),
            FailureClass::Throttled
        );
        assert_eq!(
            from_transport(
                &TransportError::Utf8(
                    String::from_utf8(vec![0xff, 0xfe]).expect_err("invalid utf-8"),
                ),
                Phase::Connect,
            ),
            FailureClass::ResponseShape
        );
    }

    #[test]
    fn shape_key_names_the_shape_class_leaf() {
        assert_eq!(
            shape_key(&provider_source(TransportError::InvalidResponse(
                "empty body".into()
            ))),
            Some(ShapeKey::MalformedResponse)
        );
        assert_eq!(
            shape_key(&provider_source(TransportError::Utf8(
                String::from_utf8(vec![0xff, 0xfe]).expect_err("invalid utf-8")
            ))),
            Some(ShapeKey::NotUtf8)
        );
        assert_eq!(
            shape_key(&provider_source(TransportError::BodyTooLarge { limit: 64 })),
            Some(ShapeKey::OversizedBody)
        );
        // A well-formed body of the wrong shape is the whole-body parse site; the same
        // category inside a stream frame is the frame site.
        assert_eq!(
            shape_key(&provider_source(ProviderError::Deserialize {
                source: json_error("[]"),
                body: "[]".into(),
            })),
            Some(ShapeKey::BodyStructure)
        );
        assert_eq!(
            shape_key(&provider_source(ProviderError::Deserialize {
                source: json_error(""),
                body: String::new(),
            })),
            None,
            "a truncated body is a read problem, not a shape"
        );
        // The frame site carries its own key: the same category read from an
        // SSE frame is a different shape from the whole-body parse site.
        assert_eq!(
            shape_key(&provider_source(TransportError::Deserialize {
                source: json_error("[]"),
                body: "[]".into(),
            })),
            Some(ShapeKey::FrameStructure)
        );
        assert_eq!(
            shape_key(&provider_source(TransportError::Deserialize {
                source: json_error(""),
                body: String::new(),
            })),
            None,
            "a truncated frame is a read problem, not a shape"
        );
    }

    /// The reset hint is read from the captured body alone, and only from the
    /// three keys the whitelist names: seconds as a number or as an `<n>s`
    /// string, at any depth. Everything else reads as "not given" rather than as
    /// a guess.
    #[test]
    fn a_reset_hint_comes_from_the_whitelisted_keys_only() {
        let now = 1_700_000_000;
        let until = |body: &str| {
            let error = provider_status(StatusCode::TOO_MANY_REQUESTS, body);
            fact_of(classify_connect(&error), &error, now).and_then(|block| block.until)
        };

        assert_eq!(until("{\"retry_after\": 120}"), Some(now + 120));
        assert_eq!(until("{\"retry_after\": \"120s\"}"), Some(now + 120));
        assert_eq!(until("{\"retry_after_seconds\": 5}"), Some(now + 5));
        assert_eq!(until("{\"retryDelay\": \"30s\"}"), Some(now + 30));
        // Any depth, and the first whitelisted key at a level wins.
        assert_eq!(until("{\"meta\": [{\"retry_after\": 7}]}"), Some(now + 7));
        assert_eq!(
            until("{\"retry_after\": 7, \"retryDelay\": 9}"),
            Some(now + 7)
        );
        // The whitelist is exact and a hint is seconds: a near-miss key, another
        // unit, a float, a boolean, and a truncated body all read as not given.
        assert_eq!(until("{\"retry_after_ms\": 1000}"), None);
        assert_eq!(until("{\"error\": {\"retry_after\": \"1m\"}}"), None);
        assert_eq!(until("{\"retry_after\": 1.5}"), None);
        assert_eq!(until("{\"retry_after\": true}"), None);
        assert_eq!(until("slow down"), None);
        assert_eq!(until("{\"retry_after\": 12"), None);
    }

    /// A settled failure reads its class rather than the table: the attempt
    /// carries no reading, so the class rows decide the exit.
    #[test]
    fn a_settled_failure_reads_its_class() {
        let episode = Episode::new(1_000);
        let attempt = Attempt::failed(
            Phase::Connect,
            FailureClass::QuotaBlocked,
            None,
            1_000,
            Duration::ZERO,
        );
        assert_eq!(
            decide(&episode.facts(&attempt), &policy(10, 1, 60, 300)),
            Decision::AdvanceFailover {
                reason: "upstream quota or payment rejection",
            }
        );
    }
}
