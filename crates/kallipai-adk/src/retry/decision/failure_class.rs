//! The failure vocabulary: which classes one send can fall into, the code and
//! retry kind each one is recorded under, the shape key a repeated failure is
//! remembered by, and how many sends each class is worth.
//!
//! "The table" in this file and in [`classify`](mod@super::classify) is the class
//! table: one row per error, read against the phase the send was in.

use kallipai_common::retry::RetryKind;

/// What a shape-class failure looked like, normalized enough to compare two of
/// them: the key comes from the parse site or the transport variant that saw it,
/// or from the status code itself, whatever the rejected response carries, when
/// the class table reads that late rejection as a shape-class failure.
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

/// Sends one episode spends on a throttle before the chain is charged: the
/// first send and one jittered resend. The episode's own budget caps the
/// allowance too, so the row can never vote past what the send path allows.
pub(super) const THROTTLE_SENDS_ALLOWED: u32 = 2;

/// Shape-class sends the episode may still perform for `key`: the per-shape
/// allowance minus the one already spent when the episode's slot holds that same
/// shape. A different key starts a fresh allowance — the slot is per shape, and
/// advancing to another endpoint replaces the whole ledger.
pub(super) fn shape_sends_allowed(previous_shape: Option<ShapeKey>, key: ShapeKey) -> u32 {
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
    /// Quota or payment rejection: 402, a 403 carrying quota evidence, or a
    /// 429 that names its reset moment.
    QuotaBlocked,
    /// A throttle the upstream left open-ended: a 429 that names no reset
    /// moment. Worth one jittered send before the chain is charged.
    Throttled,
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
            Self::Throttled => "upstream throttled the request with no reset moment",
            Self::EndpointPermanent => "endpoint or credentials rejected the request",
            Self::RequestPermanent => "the request itself was rejected",
            Self::LocalConfig => "local client configuration error",
            Self::ResponseShape => "the response shape failed twice in this episode",
            Self::Unclassified => "unclassified failure",
        }
    }

    /// The class as one stable lowercase word.
    ///
    /// This is what a span's `error.type` carries: a trace groups on it, so it
    /// never holds the prose [`Self::label`] writes for a log reader. The
    /// `match` has no catch-all arm — a class added later fails to compile
    /// here rather than reaching a dashboard as an empty code.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Transient(_) => "transient",
            Self::QuotaBlocked => "quota_blocked",
            Self::Throttled => "throttled",
            Self::EndpointPermanent => "endpoint_permanent",
            Self::RequestPermanent => "request_permanent",
            Self::LocalConfig => "local_config",
            Self::ResponseShape => "response_shape",
            Self::Unclassified => "unclassified",
        }
    }
}

/// The observation kind recorded when this class's failure leaves one: the retry
/// that follows it, or the stop it lands on. A throttle reaches both arms, so its
/// record is the one the verdict chose. `Some` marks the classes that can write
/// a record; an episode whose budget or deadline ends the round records nothing.
///
/// The retry log is an observation surface, not a budget ledger: a class with
/// `None` writes no record, and `None` does not mean that no send happens —
/// advancing to another endpoint is a real send that writes no record.
///
/// A record is a trigger observation, not proof of a further send: a quota
/// rejection writes its kind with `delay_secs: 0.0` and no wait, whether the
/// chain then advances or runs out. A shape repeat adds no record of its own:
/// the first failure of that shape already wrote one. A shape whose key cannot
/// be computed has no repeat to reach; this mapping is the same either way.
pub(crate) fn retry_kind(class: &FailureClass) -> Option<RetryKind> {
    match class {
        FailureClass::Transient(TransientKind::Transport | TransientKind::Truncated) => {
            Some(RetryKind::Transport)
        }
        FailureClass::Transient(TransientKind::Timeout) => Some(RetryKind::Timeout),
        FailureClass::Transient(TransientKind::Server) => Some(RetryKind::Server),
        FailureClass::QuotaBlocked | FailureClass::Throttled => Some(RetryKind::RateLimit),
        FailureClass::ResponseShape => Some(RetryKind::Transport),
        FailureClass::EndpointPermanent
        | FailureClass::RequestPermanent
        | FailureClass::LocalConfig
        | FailureClass::Unclassified => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Every class names a code, and every code is a lowercase word a dashboard
    /// can group on. The closed set is the compiler's (`code()` matches every
    /// variant); this test holds the form, and that no two classes share a code.
    #[test]
    fn every_failure_class_names_its_own_lowercase_code() {
        let codes: Vec<&'static str> = [
            FailureClass::Transient(TransientKind::Transport),
            FailureClass::Transient(TransientKind::Timeout),
            FailureClass::Transient(TransientKind::Server),
            FailureClass::Transient(TransientKind::Truncated),
            FailureClass::QuotaBlocked,
            FailureClass::Throttled,
            FailureClass::EndpointPermanent,
            FailureClass::RequestPermanent,
            FailureClass::LocalConfig,
            FailureClass::ResponseShape,
            FailureClass::Unclassified,
        ]
        .into_iter()
        .map(FailureClass::code)
        .collect();

        for code in &codes {
            assert!(
                !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "a code is a lowercase word: {code}"
            );
        }
        let distinct: std::collections::BTreeSet<&str> = codes.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            8,
            "one code per class, the transient kinds sharing theirs: {codes:?}"
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
            retry_kind(&FailureClass::Throttled),
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
}
