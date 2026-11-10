//! What a failure leaves in the shared upstream fact table: which classes leave
//! a [`Block`], and what that block carries — its kind, its detail, and the
//! moment service resumes when the provider named one.

use just_llm_client::{BackendError, provider_rejection};

use kallipai_common::facts::{Block, BlockKind};

use super::classify::{reset_hint, shape_key};
use super::failure_class::FailureClass;

/// The fact this class leaves behind, if the table should carry one.
///
/// The write points are the failover advances, so the mapping covers the
/// classes that reach one: a quota or payment rejection, a throttle that names
/// no reset moment, a credential or endpoint-identity rejection, and a response
/// shape that arrived twice. The other classes are either retried on the same
/// endpoint (the transient rows) or are about the request itself, which a
/// sibling endpoint would reject the same way.
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
        // 402 is the provider naming payment; 403-with-quota-evidence is
        // exhaustion. A throttle leaves the same exhaustion, with no reset.
        FailureClass::QuotaBlocked => match provider_rejection(error).map(|r| r.status) {
            Some(reqwest::StatusCode::PAYMENT_REQUIRED) => BlockKind::Payment,
            _ => BlockKind::Quota,
        },
        // A throttle names no reset moment at all, so this block carries no
        // until: the reader's window rule is what bounds it.
        FailureClass::Throttled => BlockKind::Quota,
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        classify_connect, json_error, provider_source, provider_status,
    };
    use super::*;
    use just_llm_client::TransportError;
    use reqwest::StatusCode;

    /// The throttle's fact is the exhaustion it is, with no until: the reader's
    /// window rule is what bounds it.
    #[test]
    fn a_throttle_leaves_the_quota_fact_with_no_until() {
        let now = 1_700_000_000;
        let error = provider_status(StatusCode::TOO_MANY_REQUESTS, r#"{"error":"slow down"}"#);
        assert_eq!(classify_connect(&error), FailureClass::Throttled);
        let block = fact_of(FailureClass::Throttled, &error, now).expect("a quota fact");
        assert_eq!(block.kind, BlockKind::Quota);
        assert_eq!(block.until, None, "the upstream named no moment");
    }

    /// The fact each class leaves behind: the quota and payment rows, which
    /// park the round on the spot with the structured reason, and the
    /// credential and shape rows, which stop it with the failure that caused
    /// them. The classes the table does not track leave nothing — a transient
    /// failure is the same endpoint's business, and a request-level one would
    /// fail on a sibling the same way.
    #[test]
    fn the_fact_table_rows_of_the_class_table() {
        let now = 1_700_000_000;
        let kind = |error: &BackendError| {
            fact_of(classify_connect(error), error, now).map(|block| block.kind)
        };

        // A hintless 429 is the throttle, 402 is payment: exhaustion either way.
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
}
