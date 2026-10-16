//! The terminal failure line's unit tests: the warrant, the level split,
//! the structured fields, read back from a capture subscriber. The
//! forwarding suite drives a real listener instead; these read the line
//! the terminal phase writes for one failed forward.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pingora::ErrorType;
use pingora::prelude::HttpPeer;

use super::context::{InternalFailureReason, Selection};
use super::{GatewayCtx, Route, failure_level, log_forward_failure, warrants_failure_line};
use crate::forward::dialect::Endpoint;
use crate::metrics::ForwardOutcome;
use crate::secret::{AuthStyle, ProviderCredential};

use tracing_subscriber::prelude::*;

/// The level split: every gate rejection warns (the caller's request was
/// refused), while the two forwarding failures are the gateway's own
/// trouble and record as errors.
#[test]
fn failure_levels_split_the_gates_from_the_forward_failures() {
    for outcome in [
        ForwardOutcome::GatedAuth,
        ForwardOutcome::GatedRoute,
        ForwardOutcome::GatedVisibility,
        ForwardOutcome::GatedParking,
        ForwardOutcome::GatedPayload,
        ForwardOutcome::Upstream2xx,
        ForwardOutcome::Upstream4xx,
        ForwardOutcome::Upstream5xx,
        ForwardOutcome::UpstreamOther,
    ] {
        assert_eq!(
            failure_level(outcome),
            tracing::Level::WARN,
            "{outcome:?} is not the gateway's own trouble"
        );
    }
    for outcome in [
        ForwardOutcome::ConnectError,
        ForwardOutcome::UpstreamError,
        ForwardOutcome::InternalError,
    ] {
        assert_eq!(
            failure_level(outcome),
            tracing::Level::ERROR,
            "{outcome:?} is a forwarding failure"
        );
    }
}

/// The line names the request's attribution: the endpoint, the outcome,
/// the profile the request rode, the caller's tagma and the error.
#[test]
fn the_failure_line_names_the_request_attribution() {
    let (lines, _guard) = capture_lines();
    let ctx = forwarding_ctx();
    let error = pingora::Error::new(ErrorType::ConnectRefused);
    log_forward_failure(
        Endpoint::ChatCompletions,
        ForwardOutcome::ConnectError,
        &ctx,
        Some(&error),
        Some(Duration::from_millis(7)),
    );
    let line = captured(&lines);
    assert!(line.contains("ERROR"), "a dial failure is an error: {line}");
    for field in [
        "endpoint=chat",
        "outcome=connect_error",
        "api_family=openai",
        "authority=upstream.test",
        "profile_id=profile-a",
        "tagma_id=tagma-under-test",
        "connected=false",
        "elapsed_ms=7",
        "error=",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
}

/// A gate refusal reaches the terminal phase without an error: the
/// gates answer the client themselves, so the line warrants on the
/// outcome and carries no error field.
#[test]
fn the_failure_line_omits_the_fields_a_gate_never_staged() {
    let (lines, _guard) = capture_lines();
    let ctx = GatewayCtx {
        route: Route::Forward(Endpoint::Responses),
        selection: None,
        tagma_id: Some("tagma-under-test".to_string()),
        t0: None,
        t1: None,
        pending_outcome: Some(ForwardOutcome::GatedAuth),
        pending_reason: None,
        pending_detail: None,
        upstream_status: None,
        connected: false,
    };
    log_forward_failure(
        Endpoint::Responses,
        ForwardOutcome::GatedAuth,
        &ctx,
        None,
        None,
    );
    let line = captured(&lines);
    assert!(line.contains("WARN"), "a gate rejection warns: {line}");
    assert!(line.contains("endpoint=responses"), "{line}");
    assert!(line.contains("outcome=gated_auth"), "{line}");
    assert!(line.contains("tagma_id=tagma-under-test"), "{line}");
    for absent in [
        "profile_id=",
        "authority=",
        "api_family=",
        "elapsed_ms=",
        "error=",
        "reason=",
    ] {
        assert!(!line.contains(absent), "{absent} is not staged: {line}");
    }
}

/// The warrant: an error warrants the line whatever the outcome, the
/// gates warrant it without one, and the upstream answers -- the
/// request's normal terminations -- do not.
#[test]
fn the_failure_line_warrants_on_errors_and_gates_only() {
    for outcome in [
        ForwardOutcome::Upstream2xx,
        ForwardOutcome::Upstream4xx,
        ForwardOutcome::Upstream5xx,
        ForwardOutcome::UpstreamOther,
    ] {
        assert!(
            !warrants_failure_line(outcome, false),
            "{outcome:?} is a normal termination"
        );
    }
    for outcome in [
        ForwardOutcome::GatedAuth,
        ForwardOutcome::GatedRoute,
        ForwardOutcome::GatedVisibility,
        ForwardOutcome::GatedParking,
        ForwardOutcome::GatedPayload,
    ] {
        assert!(
            warrants_failure_line(outcome, false),
            "{outcome:?} warrants without an error"
        );
        assert!(
            warrants_failure_line(outcome, true),
            "{outcome:?} warrants with one"
        );
    }
    for outcome in [
        ForwardOutcome::ConnectError,
        ForwardOutcome::UpstreamError,
        ForwardOutcome::InternalError,
    ] {
        assert!(
            warrants_failure_line(outcome, true),
            "{outcome:?} warrants through its error"
        );
    }
    // The wire-failure class splits at the error: ConnectError carries
    // one by construction, while a gateway-side internal failure answers
    // its own 5xx the way the gates do and stages no pingora error --
    // the outcome alone warrants its line.
    assert!(
        !warrants_failure_line(ForwardOutcome::ConnectError, false),
        "a connect failure warrants through its error only"
    );
    assert!(
        warrants_failure_line(ForwardOutcome::InternalError, false),
        "a gateway-side internal failure warrants on the outcome alone"
    );
}

/// A gateway-side internal failure (a store error behind the selection)
/// stages its own outcome and answers its own 5xx: the line warrants on
/// the outcome alone, records at the severity of the gateway's own
/// trouble, and names both the reason category and the failure text --
/// the client's response body carries the redacted `internal error`
/// instead.
#[test]
fn a_gateway_side_internal_failure_names_its_reason_and_text() {
    let (lines, _guard) = capture_lines();
    let mut ctx = forwarding_ctx();
    ctx.pending_reason = Some(InternalFailureReason::MissingCredential);
    ctx.pending_detail = Some("no provider credential for profile p8".to_string());
    log_forward_failure(
        Endpoint::ChatCompletions,
        ForwardOutcome::InternalError,
        &ctx,
        None,
        Some(Duration::from_millis(4)),
    );
    let line = captured(&lines);
    assert!(
        line.contains("ERROR"),
        "a gateway-side failure records at error severity: {line}"
    );
    assert!(line.contains("outcome=internal_error"), "{line}");
    assert!(line.contains("reason=missing_credential"), "{line}");
    assert!(
        line.contains("error=no provider credential for profile p8"),
        "{line}"
    );
}

/// An upstream answer without an error writes nothing: the normal
/// terminations stay off the failure stream even when the terminal
/// phase runs.
#[test]
fn an_upstream_answer_without_an_error_writes_nothing() {
    let (lines, _guard) = capture_lines();
    let mut ctx = forwarding_ctx();
    ctx.upstream_status = Some(404);
    log_forward_failure(
        Endpoint::ChatCompletions,
        ForwardOutcome::Upstream4xx,
        &ctx,
        None,
        Some(Duration::from_millis(9)),
    );
    assert!(
        captured(&lines).is_empty(),
        "an upstream answer is not a failure: {}",
        captured(&lines)
    );
}

/// The forwarding context the terminal phase sees after a selection: the
/// fields the failure line reads.
fn forwarding_ctx() -> GatewayCtx {
    GatewayCtx {
        route: Route::Forward(Endpoint::ChatCompletions),
        selection: Some(Selection {
            peer: HttpPeer::new(("127.0.0.1", 443), true, "upstream.test".to_string()),
            credential: ProviderCredential::placeholder(),
            auth: AuthStyle::Bearer,
            api_family: "openai".to_string(),
            base_path: "/v1".to_string(),
            authority: "upstream.test".to_string(),
            profile_id: "profile-a".to_string(),
        }),
        tagma_id: Some("tagma-under-test".to_string()),
        t0: Some(Instant::now()),
        t1: None,
        pending_outcome: None,
        pending_reason: None,
        pending_detail: None,
        upstream_status: None,
        connected: false,
    }
}

/// A capture subscriber: the formatted lines accumulate in the returned
/// buffer while the guard lives.
fn capture_lines() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let lines = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(crate::test_support::SharedBuf(lines.clone()))
            .with_filter(tracing_subscriber::EnvFilter::new("warn")),
    );
    let guard = tracing::subscriber::set_default(subscriber);
    (lines, guard)
}

/// The captured lines, quotes stripped: the fmt layer renders a string
/// field bare or quoted depending on how it was recorded.
fn captured(lines: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(lines.lock().unwrap().clone())
        .expect("the fmt layer writes utf8")
        .replace('"', "")
}

/// The reason vocabulary the failure line's `reason` field filters on: the
/// three values are pinned here the way the forward outcome vocabulary is
/// pinned beside its enum, so a rename cannot reach the log consumers
/// with the suite still green.
#[test]
fn internal_failure_reason_vocabulary_lock() {
    let all = [
        InternalFailureReason::StoreError,
        InternalFailureReason::MissingCredential,
        InternalFailureReason::UnparseableEndpointPrefix,
    ];
    let rendered: Vec<&str> = all.iter().map(|reason| reason.as_str()).collect();
    assert_eq!(
        rendered,
        vec![
            "store_error",
            "missing_credential",
            "unparseable_endpoint_prefix",
        ]
    );
}

/// The constructor table: the `internal` constructor lands on the
/// internal_error outcome carrying the reason it was handed and the text
/// the line records, while the rejection constructors land on their gate
/// outcome carrying neither. The rows pin that staging contract rather
/// than the inputs production hands each constructor -- a store failure
/// reaches the line through `internal_with_response`. One table, so no
/// constructor listed in it can quietly join the wrong side.
#[test]
fn the_constructors_stage_reason_and_detail_by_kind() {
    use super::routes::GateError;

    let internals = [
        (
            InternalFailureReason::StoreError,
            "database error: connection closed",
        ),
        (
            InternalFailureReason::MissingCredential,
            "no provider credential for profile p1",
        ),
        (
            InternalFailureReason::UnparseableEndpointPrefix,
            "the upstream endpoint prefix for profile p1 does not parse",
        ),
    ];
    for (reason, text) in internals {
        let error = GateError::internal(reason, text);
        assert_eq!(error.outcome, ForwardOutcome::InternalError);
        assert_eq!(error.reason, Some(reason));
        assert_eq!(error.detail.as_deref(), Some(text));
        assert_eq!(
            error.into_response().status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    let rejections = [
        (
            ForwardOutcome::GatedRoute,
            GateError::route(crate::routes::ApiError::not_found("no such profile")),
        ),
        (
            ForwardOutcome::GatedVisibility,
            GateError::visibility(crate::routes::ApiError::forbidden(
                "not visible to the caller",
            )),
        ),
        (
            ForwardOutcome::GatedParking,
            GateError::parking(crate::routes::ApiError::forbidden("profile is parked")),
        ),
    ];
    for (outcome, error) in rejections {
        assert_eq!(error.outcome, outcome);
        assert_eq!(error.reason, None, "{outcome:?} stages no reason");
        assert_eq!(error.detail, None, "{outcome:?} stages no detail");
    }
}
