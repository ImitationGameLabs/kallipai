//! Failure classification and the per-episode attempt ledger.
//!
//! Pure value logic: no I/O, no clock reads, no RNG, no store, no tracing. The
//! shell — [`super::stream_with_retry`] and the acquisition loop — reads the
//! clock, takes the random source, and persists retry records; this module maps
//! already-typed error values onto a failure class and owns the ledger that
//! counts an episode's sends.
//!
//! Why the two live together: a retry decision needs exactly two inputs — what
//! failed, and how much of the episode's budget is gone. Both are plain values
//! here, so a decision is reproducible and testable without a provider.
//!
//! The second half is the verdict: [`Episode::facts`] projects the ledger, and
//! [`decide`] reads the projection into one [`Decision`] — proceed, retry after
//! a wait, advance the failover chain, stop, or fail the round. The shell maps
//! that onto its own outcomes and never decides on its own.
//!
//! The verdict reads one more input the classifier does not: the shared upstream
//! fact table (see `kallipai_common::facts`). The shell resolves what the table
//! says about the endpoint an attempt would go to — and whether the chain has a
//! candidate to offer instead — and injects both as values (`TableReading`); a
//! fact the table carries outranks the class rows, because it is what another
//! agent already paid for.
//!
//! The classifier reads typed values only (HTTP status, provider error code,
//! `serde_json` failure category). Body text is consulted in one place: a 403
//! whose captured body carries quota evidence, because the provider reports
//! quota exhaustion there rather than in a typed field.

use std::time::Duration;

use just_llm_client::{
    BackendError, ProviderError, TransportError, captured_body, provider_rejection,
};
use kallipai_common::facts::{Block, BlockKind, BlockedFact};
use kallipai_common::retry::RetryKind;

use super::{RetryPolicy, max_sends};

/// The attempt ledger of one episode: one "first attempt → verdict" stretch of a
/// call chain.
///
/// The ledger spans the connect and mid-stream phases of the same chain (a
/// mid-stream re-entry continues it). It is replaced at every tool round, at
/// every failover advance, and before a rejected request is re-sent without its
/// images. It is a plain value on the agent context — no handle, no lock.
///
/// `attempts` is both the send counter and the retry budget unit, which is what
/// makes "sends per episode ≤ `max_retries + 1`" derivable: `Episode::after_attempt`
/// is the only counting entry point, so a send that skips the count is not a
/// shape the code can express.
///
/// Alongside the budget it carries the episode's shape memory: the key of the
/// last shape-class failure, which is what makes a repeat of the same shape an
/// endpoint-level exit rather than another retry.
#[derive(Clone, Copy, Debug)]
pub struct Episode {
    attempts: u32,
    started: u64,
    previous_shape: Option<ShapeKey>,
}

impl Episode {
    /// Start a ledger at `now` (epoch seconds). The caller reads the clock
    /// (`timefmt::now_epoch()`); this module never does.
    pub fn new(now: u64) -> Self {
        Self {
            attempts: 0,
            started: now,
            previous_shape: None,
        }
    }

    /// Requests sent inside this episode.
    pub(crate) fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Charge one send and hand back the updated ledger.
    pub(crate) fn after_attempt(&self) -> Self {
        Self {
            attempts: self.attempts.saturating_add(1),
            started: self.started,
            previous_shape: self.previous_shape,
        }
    }

    /// Record a shape-class failure: the one entry point for the shape memory,
    /// read back by [`shape_sends_allowed`].
    pub(crate) fn shape_failed(&self, key: ShapeKey) -> Self {
        Self {
            attempts: self.attempts,
            started: self.started,
            previous_shape: Some(key),
        }
    }

    /// Time on trial: `now - started`. A backwards clock (`now < started`) counts
    /// as zero rather than underflowing.
    pub(crate) fn elapsed(&self, now: u64) -> Duration {
        Duration::from_secs(now.saturating_sub(self.started))
    }

    /// The verdict input: this ledger's read-only projection plus the attempt's
    /// injected values. The only constructor of [`Facts`] — a caller cannot
    /// hand-build one, so a verdict can never be computed from a ledger and an
    /// attempt that do not belong together.
    pub(crate) fn facts(&self, attempt: &Attempt) -> Facts {
        Facts {
            attempts: self.attempts,
            elapsed: self.elapsed(attempt.now),
            previous_shape: self.previous_shape,
            phase: attempt.phase,
            failure: attempt.failure,
            shape: attempt.shape,
            jitter: attempt.jitter,
            origin: attempt.origin,
            blocked: attempt.reading.blocked.clone(),
            alternative: attempt.reading.alternative,
        }
    }
}

/// Which stretch of a call a reading was taken in: before any content flowed,
/// or after it.
///
/// `Connect`: nothing has flowed yet — a pre-send reading is taken in this phase
/// too, where the ledger alone decides. `MidStream`: content already flowed. The
/// shell decides the phase and injects it; one row of the table reads it (400/422,
/// see [`classify`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Connect,
    MidStream,
}

/// Where an attempt came from: the system (a retry, a re-run, a scheduled wake)
/// or an external wake — a user message or a task delivery.
///
/// Only the first attempt of a wake-triggered execution is `External`: a user
/// asking is served even when the shared table says the endpoint is done. Every
/// later send in that same execution is `System` again. The shell classifies the
/// attempt (the wake source lives there); the core only reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOrigin {
    System,
    External,
}

/// Whether the chain has somewhere to go: a candidate endpoint the shell
/// resolved for this attempt.
///
/// The shell resolves it — it owns the chain and the table handle — and hands
/// over the first candidate that could serve where the current endpoint cannot:
/// one the shared table has nothing to say about. What "nothing to say" means at
/// each fact level is the table's own keying, not a rule here: an account-level
/// fact answers for every endpoint on the credential, an endpoint-level one only
/// for its own endpoint. The core reads only that a candidate exists: which
/// endpoint it is, and how many were skipped, stay in the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Alternative;

/// What the shared table says about one attempt: the fact the endpoint it would
/// go to already carries, and whether the chain has a candidate to offer
/// instead.
///
/// The shell resolves both from the table handle and the chain. `none` is the
/// reading an attempt carries when there is no table to read, or when the
/// reading was taken elsewhere: the pre-send reading is the one that carries
/// facts.
#[derive(Clone, Debug)]
pub(crate) struct TableReading {
    /// The readable fact for the endpoint the attempt would go to, if any, with
    /// the account hint the shell's log line carries.
    pub(crate) blocked: Option<BlockedFact>,
    /// The candidate the shell resolved, when one exists.
    pub(crate) alternative: Option<Alternative>,
}

impl TableReading {
    /// No table, or nothing to say: fail-open, so a reader that cannot reach
    /// the table behaves as if there were no facts.
    pub(crate) fn none() -> Self {
        Self {
            blocked: None,
            alternative: None,
        }
    }
}

/// One attempt's injected values: what the shell observed, in the shape the
/// verdict reads.
///
/// `failure: None` is the pre-send reading — nothing has failed yet, so the
/// verdict is about the ledger and the shared table alone. A settled failure
/// carries no table reading and no wake origin: its exit is the class rows'
/// business, and the reading belongs to the decision of whether to send at all.
#[derive(Clone, Debug)]
pub(crate) struct Attempt {
    phase: Phase,
    origin: AttemptOrigin,
    failure: Option<FailureClass>,
    /// The shape key of this failure, when the error carries one. Computed by
    /// the shell (a shape is a property of the wire form), compared here.
    shape: Option<ShapeKey>,
    now: u64,
    jitter: Duration,
    reading: TableReading,
}

impl Attempt {
    /// An attempt that has not been sent: the pre-send reading, which carries
    /// what the table says about the endpoint it would go to.
    pub(crate) fn pre_send(
        phase: Phase,
        origin: AttemptOrigin,
        now: u64,
        reading: TableReading,
    ) -> Self {
        Self {
            phase,
            origin,
            failure: None,
            shape: None,
            now,
            jitter: Duration::ZERO,
            reading,
        }
    }

    /// An attempt that failed: the class the table gave it, its shape key when
    /// it has one, and the values the shell injected.
    ///
    /// System-origin and no table reading: a failure that already happened is
    /// not the wake-triggered first attempt, and its exit is decided by the
    /// class rows rather than by whether a send should happen at all.
    pub(crate) fn failed(
        phase: Phase,
        failure: FailureClass,
        shape: Option<ShapeKey>,
        now: u64,
        jitter: Duration,
    ) -> Self {
        Self {
            phase,
            origin: AttemptOrigin::System,
            failure: Some(failure),
            shape,
            now,
            jitter,
            reading: TableReading::none(),
        }
    }
}

/// The verdict input. Fields are private to this module: the only way to build
/// one is [`Episode::facts`].
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    attempts: u32,
    elapsed: Duration,
    previous_shape: Option<ShapeKey>,
    phase: Phase,
    failure: Option<FailureClass>,
    shape: Option<ShapeKey>,
    jitter: Duration,
    origin: AttemptOrigin,
    blocked: Option<BlockedFact>,
    alternative: Option<Alternative>,
}

/// What the shell does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The pre-send reading passed: this send may happen.
    Proceed,
    /// Send again after `after`, which is already clamped to what is left of the
    /// episode's deadline — the shell sleeps it as it stands.
    Retry { after: Duration },
    /// The endpoint cannot serve this request: advance the failover chain. A new
    /// episode starts with the new endpoint.
    AdvanceFailover { reason: &'static str },
    /// Stop sending. `kind` says whether the chain ran out — the outer re-run
    /// budget may still retry the round later — or whether no send can help.
    Stop {
        kind: StopKind,
        reason: &'static str,
    },
    /// Fail the round: no profile can serve this request.
    Fatal { reason: &'static str },
}

/// Which stopping verdict this is: the shell arms a re-run only for the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopKind {
    /// The episode's own budget or deadline ran out: the shell walks the chain,
    /// and a chain with no candidate left ends the round.
    ChainExhausted,
    /// A local configuration error: it fails on every profile, so a switch
    /// cannot help and the round is not re-run.
    ExternalOnly,
}

/// What a shape-class failure looked like, normalized enough to compare two of
/// them: the key comes from the parse site or the transport variant that saw it,
/// or from the status code itself, whatever the rejected response carries, when
/// the table reads that late rejection as a shape-class failure.
///
/// The key is coarse by construction, so two failures that differ only in
/// field-level detail still read as the same shape, and the repeat is what the
/// gate catches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShapeKey {
    /// The provider's whole-body parse: well-formed JSON, wrong structure.
    BodyStructure,
    /// The transport's per-frame parse: an SSE frame that parsed but did not fit.
    FrameStructure,
    /// A body that was not valid UTF-8.
    NotUtf8,
    /// A body over the transport's size cap.
    OversizedBody,
    /// The response did not look like a response at all.
    MalformedResponse,
    /// The provider rejected the request with a status the table reads as a
    /// shape once content had flowed: the rejection itself is the wire form the
    /// repeat gate compares.
    StatusRejected(reqwest::StatusCode),
}

/// Shape-class retries allowed for one shape key, per endpoint, per episode.
const SHAPE_REPEAT_ALLOWED: u32 = 1;

/// Shape-class sends the episode may still perform for `key`: the per-shape
/// allowance minus the one already spent when the episode's slot holds that same
/// shape. A different key starts a fresh allowance — the slot is per shape, and
/// advancing to another endpoint replaces the whole ledger.
fn shape_sends_allowed(previous_shape: Option<ShapeKey>, key: ShapeKey) -> u32 {
    let spent = u32::from(previous_shape == Some(key));
    SHAPE_REPEAT_ALLOWED.saturating_sub(spent)
}

/// Why a send failed, at the granularity the exit mapping needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransientKind {
    /// The connection failed, or the body could not be read.
    Transport,
    /// The provider answered 408.
    Timeout,
    /// The provider answered 5xx.
    Server,
    /// The response body or a stream frame was cut short.
    Truncated,
}

/// The classes one send can fall into — the single classification enum of the
/// retry path. The persisted [`RetryKind`] is derived from it by [`retry_kind`],
/// never produced by a second classifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// Worth another send after a backoff.
    Transient(TransientKind),
    /// Quota or payment rejection: 429, 402, or a 403 carrying quota evidence.
    QuotaBlocked,
    /// The endpoint or its credentials cannot serve this request.
    EndpointPermanent,
    /// The request itself is bad: resending it changes nothing.
    RequestPermanent,
    /// Local configuration or client construction failure.
    LocalConfig,
    /// The response did not have the shape the client expects.
    ResponseShape,
    /// Nothing in the table matched, including variants upstream may add.
    Unclassified,
}

impl FailureClass {
    /// The class, named once for every exit that reports it: the stop message,
    /// the failover reason, and the fail-the-round log line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Transient(_) => "transient failure",
            Self::QuotaBlocked => "upstream quota or payment rejection",
            Self::EndpointPermanent => "endpoint or credentials rejected the request",
            Self::RequestPermanent => "the request itself was rejected",
            Self::LocalConfig => "local client configuration error",
            Self::ResponseShape => "the response shape failed twice in this episode",
            Self::Unclassified => "unclassified failure",
        }
    }
}

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
        // Quota and payment rejections share one class: neither recovers by
        // resending the same request.
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

/// The observation kind recorded when this class's failure leaves one: the retry
/// that follows it, or the stop it lands on (a quota rejection never reaches the
/// retry arm, so its record is the stop). `Some` marks the classes that can write
/// a record; an episode whose budget or deadline ends the round records nothing.
///
/// The retry log is an observation surface, not a budget ledger: a class with
/// `None` writes no record, and `None` does not mean that no send happens —
/// advancing to another endpoint is a real send that writes no record.
///
/// A record is a trigger observation, not proof of a further send: a quota
/// rejection writes its kind with `delay_secs: 0.0` and no wait, whether the
/// chain then advances or runs out. A shape repeat adds no record of its own:
/// the first failure of that shape already wrote one.
pub(crate) fn retry_kind(class: &FailureClass) -> Option<RetryKind> {
    match class {
        FailureClass::Transient(TransientKind::Transport | TransientKind::Truncated) => {
            Some(RetryKind::Transport)
        }
        FailureClass::Transient(TransientKind::Timeout) => Some(RetryKind::Timeout),
        FailureClass::Transient(TransientKind::Server) => Some(RetryKind::Server),
        FailureClass::QuotaBlocked => Some(RetryKind::RateLimit),
        FailureClass::ResponseShape => Some(RetryKind::Transport),
        FailureClass::EndpointPermanent
        | FailureClass::RequestPermanent
        | FailureClass::LocalConfig
        | FailureClass::Unclassified => None,
    }
}

/// The keys a provider may use for "try again at". Only these three, at any
/// depth: the whitelist is what keeps an unrelated number in a body from
/// reading as a reset hint.
const RESET_HINT_KEYS: [&str; 3] = ["retry_after", "retryDelay", "retry_after_seconds"];

/// The fact this class leaves behind, if the table should carry one.
///
/// The write points are the failover advances, so the mapping covers the
/// classes that reach one: a quota or payment rejection, a credential or
/// endpoint-identity rejection, and a response shape that arrived twice. The
/// other classes are either retried on the same endpoint (the transient rows)
/// or are about the request itself, which a sibling endpoint would reject the
/// same way.
///
/// A shape failure reaches a write point only on its repeat, so
/// [`FailureClass::ResponseShape`] maps unconditionally: the first failure of
/// a shape that carries a key takes the retry arm and writes nothing. A shape
/// whose key cannot be computed has nothing to compare and reaches a write
/// point on its first failure, as the `decide` row says; no producer of this
/// class is keyless today.
///
/// `until` is the upstream's own reset hint when it gave one, and `status` the
/// upstream's own answer when there was a response at all; the trust bound is
/// the reader's business, not this function's.
pub(crate) fn fact_of(class: FailureClass, error: &BackendError, now: u64) -> Option<Block> {
    let kind = match class {
        // 402 is the provider naming payment; 429 — and a 403 carrying quota
        // evidence — is exhaustion. One class, two facts.
        FailureClass::QuotaBlocked => match provider_rejection(error).map(|r| r.status) {
            Some(reqwest::StatusCode::PAYMENT_REQUIRED) => BlockKind::Payment,
            _ => BlockKind::Quota,
        },
        FailureClass::EndpointPermanent => BlockKind::EndpointAuth,
        FailureClass::ResponseShape => BlockKind::EndpointShape,
        FailureClass::Transient(_)
        | FailureClass::RequestPermanent
        | FailureClass::LocalConfig
        | FailureClass::Unclassified => return None,
    };
    Some(Block {
        kind,
        until: reset_hint(error, now),
        detail: detail_of(class, error),
        status: provider_rejection(error).map(|rejection| rejection.status.as_u16()),
        recorded_at: now,
    })
}

/// The upstream's reset hint as an absolute moment, when it gave one.
///
/// Read from the captured body alone: the hint arrives as provider-specific
/// JSON, so the body is parsed and the whitelisted keys are searched at any
/// depth. Everything else — no body, a truncated or non-JSON body, a value in a
/// shape the whitelist does not describe — reads as "not given" rather than as a
/// guess.
fn reset_hint(error: &BackendError, now: u64) -> Option<u64> {
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

/// The bounded excerpt the fact carries. Everything but the shape rows is the
/// provider's own words, truncated once for every reader that shows them; a
/// shape row has no body worth quoting, so it names the shape and the provider's
/// own error code instead.
fn detail_of(class: FailureClass, error: &BackendError) -> String {
    if class != FailureClass::ResponseShape {
        return crate::llm_error::render_error(error);
    }
    let key = shape_key(error)
        .map(|key| format!("{key:?}"))
        .unwrap_or_else(|| "unknown".to_string());
    match provider_rejection(error).and_then(|rejection| rejection.code) {
        Some(code) => format!("shape {key}, provider code {code}"),
        None => format!("shape {key}"),
    }
}

/// Why the episode stopped sending: its own budget, in the phase it was spent.
const BUDGET_EXHAUSTED: &str = "retry budget exhausted";
const BUDGET_EXHAUSTED_MID_STREAM: &str = "mid-stream retry budget exhausted";
const DEADLINE_EXHAUSTED: &str = "retry deadline exhausted";
/// The shape row with no key to compare: the failure cannot be named, so the
/// row reads as a repeat.
const SHAPE_UNIDENTIFIED: &str = "unidentifiable response shape, so another send would repeat it";
/// The upper bound a wait's jitter is drawn from. The draw itself happens in the
/// shell: this module keeps the bound as a value so the schedule stays pure.
pub(crate) const RETRY_JITTER_MAX: Duration = Duration::from_secs(1);

/// The backoff schedule before clamping: `min(base * 2^n, max_delay) + jitter`,
/// with `n` the exponent the caller supplies: the sends already made in the
/// episode for a retry, or the outer re-run index for the round-level wait.
/// The jitter rides outside the cap, so the capped plateau carries it too;
/// `max_delay` stays the schedule's own upper bound.
pub(crate) fn backoff_schedule(policy: &RetryPolicy, exponent: u32, jitter: Duration) -> Duration {
    let exp_delay = policy
        .base_delay
        .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
    exp_delay.min(policy.max_delay).saturating_add(jitter)
}

/// The wait a retry performs: the schedule, clamped to what is left of the
/// episode's deadline. The clamp lives here, not in the shell, so no caller can
/// sleep past the deadline — and `saturating_sub` keeps a backwards clock from
/// underflowing it.
fn retry_after(facts: &Facts, policy: &RetryPolicy) -> Duration {
    let exponent = facts.attempts.saturating_sub(1);
    backoff_schedule(policy, exponent, facts.jitter)
        .min(policy.retry_timeout.saturating_sub(facts.elapsed))
}

/// The verdict for one attempt: what the shell does next. The single exit of
/// the retry logic.
///
/// Order, fixed: the shared table first, then the pre-send reading (nothing has
/// failed yet), then the class rows, and inside the retry row the episode's
/// budget before its deadline. A class row outranks that gate: a failure another
/// send cannot help with leaves through its own exit even on the episode's last
/// send. A fact outranks the class rows, because it is what another agent
/// already paid for.
pub(crate) fn decide(facts: &Facts, policy: &RetryPolicy) -> Decision {
    // A fact the endpoint already carries: this attempt does not send. The one
    // exception is the wake-triggered execution's first attempt, which sends
    // exactly once anyway — a user asking is always served, at the price of one
    // bounded attempt on an endpoint that may still refuse.
    if let Some(fact) = facts.blocked.as_ref()
        && !(facts.origin == AttemptOrigin::External && facts.failure.is_none())
    {
        return blocked_exit(facts, &fact.block);
    }
    let Some(class) = facts.failure else {
        return if facts.attempts >= max_sends(policy) {
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: budget_exhausted_reason(facts.phase),
            }
        } else {
            Decision::Proceed
        };
    };
    match class {
        FailureClass::Transient(_) => retry_or_stop(facts, policy),
        // A shape-class failure is endpoint-level once the same shape has
        // already failed in this episode; a shape whose key cannot be computed
        // has no comparison to make, so it reads the same way.
        FailureClass::ResponseShape => match facts.shape {
            Some(key) if shape_sends_allowed(facts.previous_shape, key) > 0 => {
                retry_or_stop(facts, policy)
            }
            Some(_) => Decision::AdvanceFailover {
                reason: class.label(),
            },
            None => Decision::AdvanceFailover {
                reason: SHAPE_UNIDENTIFIED,
            },
        },
        FailureClass::QuotaBlocked | FailureClass::EndpointPermanent => Decision::AdvanceFailover {
            reason: class.label(),
        },
        FailureClass::LocalConfig => Decision::Stop {
            kind: StopKind::ExternalOnly,
            reason: class.label(),
        },
        FailureClass::RequestPermanent | FailureClass::Unclassified => Decision::Fatal {
            reason: class.label(),
        },
    }
}

/// The exit a fact leaves: advance when the chain has a candidate, stop when it
/// does not. The candidate was resolved by the shell against the fact's own
/// level, so an account-level fact only ever finds a candidate on another
/// account.
fn blocked_exit(facts: &Facts, block: &Block) -> Decision {
    let reason = fact_reason(block.kind);
    match facts.alternative {
        Some(_) => Decision::AdvanceFailover { reason },
        None => Decision::Stop {
            kind: StopKind::ExternalOnly,
            reason,
        },
    }
}

/// What a fact says, in the operator's words: the reason a stop or a switch
/// reports. One wording per kind, because the reader acts differently on each —
/// an exhausted account is waited out, a rejected endpoint is replaced.
fn fact_reason(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Quota => "upstream quota exhausted for this account",
        BlockKind::Payment => "upstream payment required for this account",
        BlockKind::EndpointAuth => "endpoint credentials rejected",
        BlockKind::EndpointShape => "endpoint response shape rejected",
    }
}

/// The retry row's own gate: the episode's budget first, its deadline second,
/// and a wait only if both allow it. A failure that ends the episode schedules
/// no retry, so nothing is recorded for a send that will not happen.
fn retry_or_stop(facts: &Facts, policy: &RetryPolicy) -> Decision {
    if facts.attempts >= max_sends(policy) {
        return Decision::Stop {
            kind: StopKind::ChainExhausted,
            reason: budget_exhausted_reason(facts.phase),
        };
    }
    if facts.elapsed >= policy.retry_timeout {
        return Decision::Stop {
            kind: StopKind::ChainExhausted,
            reason: DEADLINE_EXHAUSTED,
        };
    }
    Decision::Retry {
        after: retry_after(facts, policy),
    }
}

/// The budget reason names the phase: a mid-stream drop spends the same
/// allowance as a prepare/send failure, but reads differently in the log.
fn budget_exhausted_reason(phase: Phase) -> &'static str {
    match phase {
        Phase::Connect => BUDGET_EXHAUSTED,
        Phase::MidStream => BUDGET_EXHAUSTED_MID_STREAM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    /// A rejection that reached the provider, in the shape the client produces:
    /// the status rides a `TransportError` inside the boxed source chain.
    fn provider_status(status: StatusCode, body: &str) -> BackendError {
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
    fn provider_source<E>(source: E) -> BackendError
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        BackendError::provider("openai-compatible", source)
    }

    /// A `serde_json` failure of the category the text produces: `Data` (wrong
    /// shape), `Eof` (truncated), `Syntax` (malformed).
    fn json_error(text: &str) -> serde_json::Error {
        serde_json::from_str::<u32>(text).expect_err("not a number")
    }

    /// The table's connect-phase reading, the phase most cases exercise; the
    /// mid-stream row has its own case below.
    fn classify_connect(error: &BackendError) -> FailureClass {
        classify(error, Phase::Connect)
    }

    /// A verdict input for [`decide`]: the ledger's projection with the injected
    /// attempt values. `jitter` is zero unless a case asks for it, so the
    /// expected waits are exact.
    fn facts(
        episode: &Episode,
        phase: Phase,
        failure: Option<FailureClass>,
        shape: Option<ShapeKey>,
        jitter: Duration,
    ) -> Facts {
        let attempt = match failure {
            Some(class) => Attempt::failed(phase, class, shape, 1_000, jitter),
            None => Attempt::pre_send(phase, AttemptOrigin::System, 1_000, TableReading::none()),
        };
        episode.facts(&attempt)
    }

    /// A fact the shared table carries for the endpoint, with the hint a log
    /// line shows.
    fn blocked(kind: BlockKind, until: Option<u64>) -> BlockedFact {
        BlockedFact {
            block: Block {
                kind,
                until,
                detail: "seeded evidence".to_owned(),
                status: None,
                recorded_at: 1_000,
            },
            account_hint: "01234567".to_owned(),
        }
    }

    /// The pre-send reading: what the table says, and whether the shell found a
    /// candidate to move to.
    fn reading(blocked: Option<BlockedFact>, alternative: Option<Alternative>) -> TableReading {
        TableReading {
            blocked,
            alternative,
        }
    }

    /// The verdict for a pre-send reading at `now`.
    fn pre_send_verdict(
        origin: AttemptOrigin,
        reading: TableReading,
        policy: &RetryPolicy,
    ) -> Decision {
        let episode = Episode::new(1_000);
        let attempt = Attempt::pre_send(Phase::Connect, origin, 1_000, reading);
        decide(&episode.facts(&attempt), policy)
    }

    fn policy(max_retries: u32, base: u64, max: u64, timeout: u64) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::from_secs(base),
            max_delay: Duration::from_secs(max),
            retry_timeout: Duration::from_secs(timeout),
        }
    }

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
    fn a_pre_send_verdict_reads_the_ledger_alone() {
        let policy = policy(2, 1, 5, 60);
        let fresh = Episode::new(1_000);
        assert_eq!(
            decide(
                &facts(&fresh, Phase::Connect, None, None, Duration::ZERO),
                &policy
            ),
            Decision::Proceed
        );

        let spent = fresh.after_attempt().after_attempt().after_attempt();
        assert_eq!(
            decide(
                &facts(&spent, Phase::Connect, None, None, Duration::ZERO),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: "retry budget exhausted"
            },
            "a spent episode may not send"
        );
    }

    #[test]
    fn a_transient_failure_retries_until_the_episode_is_spent() {
        let policy = policy(2, 1, 5, 60);
        let transient = FailureClass::Transient(TransientKind::Server);
        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &first,
                    Phase::Connect,
                    Some(transient),
                    None,
                    Duration::from_millis(500)
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_millis(1_500)
            },
            "the first retry waits one base delay plus the injected jitter"
        );

        // The last send of the episode: its failure exhausts the budget, and the
        // reason names the phase it failed in.
        let last = Episode::new(1_000)
            .after_attempt()
            .after_attempt()
            .after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &last,
                    Phase::MidStream,
                    Some(transient),
                    None,
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: "mid-stream retry budget exhausted"
            }
        );

        // An episode older than `retry_timeout` stops before any wait.
        let stale = Episode::new(900).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &stale,
                    Phase::Connect,
                    Some(transient),
                    None,
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: "retry deadline exhausted"
            }
        );
    }

    #[test]
    fn the_wait_is_clamped_to_the_remaining_deadline() {
        // The schedule asks for 30s; two seconds of the episode's deadline are left.
        let policy = policy(3, 30, 30, 10);
        let episode = Episode::new(992).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &episode,
                    Phase::Connect,
                    Some(FailureClass::Transient(TransientKind::Transport)),
                    None,
                    Duration::from_secs(5)
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(2)
            }
        );
    }

    #[test]
    fn episode_counts_sends_and_measures_time_on_trial() {
        let episode = Episode::new(1_000);
        assert_eq!(episode.attempts(), 0);
        assert_eq!(episode.elapsed(1_005), Duration::from_secs(5));

        let charged = episode.after_attempt();
        assert_eq!(charged.attempts(), 1);
        assert_eq!(
            charged.elapsed(1_005),
            Duration::from_secs(5),
            "the deadline runs from the episode's start, not the last attempt"
        );
        assert_eq!(episode.attempts(), 0, "charging returns a new ledger");
    }

    #[test]
    fn episode_elapsed_treats_a_backwards_clock_as_zero() {
        let episode = Episode::new(1_000);
        assert_eq!(episode.elapsed(999), Duration::ZERO);
    }

    #[test]
    fn episode_attempt_count_saturates() {
        let episode = Episode {
            attempts: u32::MAX,
            started: 0,
            previous_shape: None,
        };
        assert_eq!(episode.after_attempt().attempts(), u32::MAX);
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
            FailureClass::QuotaBlocked
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
    fn retry_kind_names_the_record_each_class_writes() {
        assert_eq!(
            retry_kind(&FailureClass::Transient(TransientKind::Transport)),
            Some(RetryKind::Transport)
        );
        assert_eq!(
            retry_kind(&FailureClass::Transient(TransientKind::Truncated)),
            Some(RetryKind::Transport)
        );
        assert_eq!(
            retry_kind(&FailureClass::Transient(TransientKind::Timeout)),
            Some(RetryKind::Timeout)
        );
        assert_eq!(
            retry_kind(&FailureClass::Transient(TransientKind::Server)),
            Some(RetryKind::Server)
        );
        assert_eq!(
            retry_kind(&FailureClass::QuotaBlocked),
            Some(RetryKind::RateLimit)
        );
        assert_eq!(
            retry_kind(&FailureClass::ResponseShape),
            Some(RetryKind::Transport)
        );
        assert_eq!(retry_kind(&FailureClass::EndpointPermanent), None);
        assert_eq!(retry_kind(&FailureClass::RequestPermanent), None);
        assert_eq!(retry_kind(&FailureClass::LocalConfig), None);
        assert_eq!(retry_kind(&FailureClass::Unclassified), None);
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
    /// A late status rejection carries a key, so the shape row's one retry is
    /// real: the first late 400 retries, the same status after that advances.
    #[test]
    fn a_late_status_rejection_gets_one_shape_retry() {
        let policy = policy(3, 1, 5, 60);
        let error = provider_status(StatusCode::BAD_REQUEST, "");
        assert_eq!(
            shape_key(&error),
            Some(ShapeKey::StatusRejected(StatusCode::BAD_REQUEST))
        );
        let class = classify(&error, Phase::MidStream);
        assert_eq!(class, FailureClass::ResponseShape);

        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &first,
                    Phase::MidStream,
                    Some(class),
                    shape_key(&error),
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "the first late rejection gets the shape row's one retry"
        );

        // The slot now holds that status, so the same rejection is endpoint-level.
        let spent = first.shape_failed(ShapeKey::StatusRejected(StatusCode::BAD_REQUEST));
        assert_eq!(
            decide(
                &facts(
                    &spent,
                    Phase::MidStream,
                    Some(class),
                    shape_key(&error),
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: class.label()
            }
        );
    }

    #[test]
    fn the_shape_allowance_is_per_key_and_spent_once() {
        assert_eq!(shape_sends_allowed(None, ShapeKey::BodyStructure), 1);
        let spent = Some(ShapeKey::BodyStructure);
        assert_eq!(shape_sends_allowed(spent, ShapeKey::BodyStructure), 0);
        assert_eq!(
            shape_sends_allowed(spent, ShapeKey::FrameStructure),
            1,
            "a different shape starts a fresh allowance"
        );
    }
    #[test]
    fn the_jitter_rides_outside_the_cap() {
        let policy = policy(3, 1, 1, 60);
        let episode = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &episode,
                    Phase::Connect,
                    Some(FailureClass::Transient(TransientKind::Server)),
                    None,
                    Duration::from_millis(700)
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_millis(1_700)
            },
            "the capped plateau carries the jitter too"
        );
    }

    #[test]
    fn the_class_rows_pick_the_exit() {
        let policy = policy(3, 1, 5, 60);
        let episode = Episode::new(1_000).after_attempt();
        let verdict = |class: FailureClass| {
            decide(
                &facts(&episode, Phase::Connect, Some(class), None, Duration::ZERO),
                &policy,
            )
        };

        assert_eq!(
            verdict(FailureClass::QuotaBlocked),
            Decision::AdvanceFailover {
                reason: "upstream quota or payment rejection"
            }
        );
        assert_eq!(
            verdict(FailureClass::EndpointPermanent),
            Decision::AdvanceFailover {
                reason: "endpoint or credentials rejected the request"
            }
        );
        assert_eq!(
            verdict(FailureClass::LocalConfig),
            Decision::Stop {
                kind: StopKind::ExternalOnly,
                reason: "local client configuration error"
            },
            "a local misconfiguration fails on every profile: stop, do not switch"
        );
        assert_eq!(
            verdict(FailureClass::RequestPermanent),
            Decision::Fatal {
                reason: "the request itself was rejected"
            }
        );
        assert_eq!(
            verdict(FailureClass::Unclassified),
            Decision::Fatal {
                reason: "unclassified failure"
            },
            "an unknown failure is not retried"
        );
    }

    #[test]
    fn a_shape_failure_retries_once_then_reads_as_endpoint_level() {
        let policy = policy(3, 1, 5, 60);
        let episode = Episode::new(1_000).after_attempt();
        let shape = FailureClass::ResponseShape;
        let body = Some(ShapeKey::BodyStructure);
        assert_eq!(
            decide(
                &facts(&episode, Phase::Connect, Some(shape), body, Duration::ZERO),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "the first failure of a shape retries once"
        );

        let spent = episode.shape_failed(ShapeKey::BodyStructure);
        assert_eq!(
            decide(
                &facts(&spent, Phase::Connect, Some(shape), body, Duration::ZERO),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: "the response shape failed twice in this episode"
            }
        );
        assert_eq!(
            decide(
                &facts(
                    &spent,
                    Phase::Connect,
                    Some(shape),
                    Some(ShapeKey::FrameStructure),
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "a different shape starts a fresh allowance"
        );
        assert_eq!(
            decide(
                &facts(&episode, Phase::Connect, Some(shape), None, Duration::ZERO),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: "unidentifiable response shape, so another send would repeat it"
            },
            "a shape whose key cannot be computed has no repeat to compare"
        );
    }
    /// The fact each class leaves behind: the account-level rows (quota, payment)
    /// and the endpoint-level rows (credentials, shape). The classes the table
    /// does not track leave nothing — a transient failure is the same endpoint's
    /// business, and a request-level one would fail on a sibling the same way.
    #[test]
    fn the_fact_table_rows_of_the_class_table() {
        let now = 1_700_000_000;
        let kind = |error: &BackendError| {
            fact_of(classify_connect(error), error, now).map(|block| block.kind)
        };

        // 429 is exhaustion, 402 is payment: one class, two facts.
        let quota = provider_status(StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"slow down\"}");
        assert_eq!(kind(&quota), Some(BlockKind::Quota));
        let payment = provider_status(StatusCode::PAYMENT_REQUIRED, "{\"error\":\"pay up\"}");
        assert_eq!(kind(&payment), Some(BlockKind::Payment));
        // A 403 reads by its own words: quota evidence, or the credential.
        let forbidden_quota =
            provider_status(StatusCode::FORBIDDEN, "{\"error\":\"quota exceeded\"}");
        assert_eq!(kind(&forbidden_quota), Some(BlockKind::Quota));
        let forbidden = provider_status(StatusCode::FORBIDDEN, "{\"error\":\"bad key\"}");
        assert_eq!(kind(&forbidden), Some(BlockKind::EndpointAuth));
        let unauthorized = provider_status(StatusCode::UNAUTHORIZED, "{}");
        assert_eq!(kind(&unauthorized), Some(BlockKind::EndpointAuth));
        let missing = provider_status(StatusCode::NOT_FOUND, "{}");
        assert_eq!(kind(&missing), Some(BlockKind::EndpointAuth));
        // The shape row is written on the repeat, so it maps whenever the class
        // reaches a write point at all.
        let shape = provider_source(TransportError::InvalidResponse("empty body".into()));
        assert_eq!(classify_connect(&shape), FailureClass::ResponseShape);
        assert_eq!(kind(&shape), Some(BlockKind::EndpointShape));

        let timeout = provider_status(StatusCode::REQUEST_TIMEOUT, "{}");
        assert_eq!(kind(&timeout), None);
        let server = provider_status(StatusCode::INTERNAL_SERVER_ERROR, "{}");
        assert_eq!(kind(&server), None);
        let bad_request = provider_status(StatusCode::BAD_REQUEST, "{}");
        assert_eq!(kind(&bad_request), None);
        assert_eq!(kind(&provider_source(json_error("not json"))), None);
    }

    /// A fact carries its own observation: when it was seen, the provider's own
    /// words, and no reset hint when the upstream gave none.
    #[test]
    fn a_fact_carries_its_own_observation() {
        let now = 1_700_000_000;
        let error = provider_status(StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"slow down\"}");
        let block = fact_of(classify_connect(&error), &error, now).expect("a quota fact");
        assert_eq!(block.recorded_at, now);
        assert_eq!(block.until, None, "this body names no reset");
        assert!(
            block.detail.contains("slow down"),
            "the fact quotes the provider: {}",
            block.detail
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

    /// A fact the table carries outranks the ledger: a system-initiated attempt
    /// does not send, and with nowhere to go it stops on the fact's own wording.
    #[test]
    fn a_fact_the_table_carries_stops_a_system_attempt() {
        let verdict = pre_send_verdict(
            AttemptOrigin::System,
            reading(Some(blocked(BlockKind::Quota, None)), None),
            &policy(10, 1, 60, 300),
        );
        assert_eq!(
            verdict,
            Decision::Stop {
                kind: StopKind::ExternalOnly,
                reason: "upstream quota exhausted for this account",
            }
        );
    }

    /// The same hit with a candidate the shell resolved: the chain advances, and
    /// the reason still names the fact.
    #[test]
    fn a_fact_with_a_candidate_advances_the_chain() {
        let verdict = pre_send_verdict(
            AttemptOrigin::System,
            reading(
                Some(blocked(BlockKind::EndpointShape, None)),
                Some(Alternative),
            ),
            &policy(10, 1, 60, 300),
        );
        assert_eq!(
            verdict,
            Decision::AdvanceFailover {
                reason: "endpoint response shape rejected",
            }
        );
    }

    /// The wake-triggered execution's first attempt sends anyway: a user asking is
    /// served once, whatever the table says.
    #[test]
    fn the_wake_first_attempt_sends_through_a_fact() {
        let verdict = pre_send_verdict(
            AttemptOrigin::External,
            reading(Some(blocked(BlockKind::Quota, None)), None),
            &policy(10, 1, 60, 300),
        );
        assert_eq!(verdict, Decision::Proceed);
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

    /// Every kind the table can carry has its own wording, because the reader
    /// acts differently on each.
    #[test]
    fn every_fact_kind_names_its_own_reason() {
        assert_eq!(
            fact_reason(BlockKind::Quota),
            "upstream quota exhausted for this account"
        );
        assert_eq!(
            fact_reason(BlockKind::Payment),
            "upstream payment required for this account"
        );
        assert_eq!(
            fact_reason(BlockKind::EndpointAuth),
            "endpoint credentials rejected"
        );
        assert_eq!(
            fact_reason(BlockKind::EndpointShape),
            "endpoint response shape rejected"
        );
    }
}
