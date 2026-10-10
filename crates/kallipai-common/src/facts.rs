//! Shared upstream-failure facts: what one agent learned about an endpoint or
//! an account, readable by every agent.
//!
//! A fact is written when a failure verdict says the endpoint or the account
//! cannot serve the request (quota, payment, credentials, a repeated response
//! shape). The table lives in the tagma and is keyed by the credential
//! fingerprint — an identity the callers never see: they hand over a provider
//! id and get a verdict, or a display row, back.
//!
//! This module carries the value types and the one hit predicate every reader
//! shares. The implementation (key computation, storage, capacity) is the
//! tagma's; the adk only holds the handle.

use serde::{Deserialize, Serialize};

/// How long a fact without an upstream reset hint stays readable: the
/// conservative window a writer leaves behind when it has no better answer.
pub const FACT_TTL_MAX_SECS: u64 = 3600;

/// How many facts the table keeps. Oldest by `recorded_at` is evicted when a
/// new key arrives at capacity, so a long-lived tagma cannot grow without bound.
pub const FACT_TABLE_CAPACITY: usize = 256;

/// Trust bound on an upstream reset hint, for display only: a reset further out
/// than this is shown as unknown. It never extends or shortens a fact's life —
/// [`is_active`] does not read it.
pub const QUOTA_RESET_TRUST_MAX_SECS: u64 = 86_400;

/// What the upstream rejected.
///
/// The first two are account-level (every endpoint sharing the credential is
/// affected); the last two are endpoint-level (a sibling endpoint may serve).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Quota exhaustion (HTTP 429, or a 403 carrying quota evidence).
    Quota,
    /// Payment required (HTTP 402).
    Payment,
    /// The endpoint rejected the credentials (HTTP 401/403, or 404).
    EndpointAuth,
    /// The same malformed response shape arrived twice in one episode.
    EndpointShape,
}

impl BlockKind {
    /// Whether a sibling endpoint can serve despite this fact. Account-level
    /// facts follow the credential, so only a different account clears them.
    pub fn is_account_level(self) -> bool {
        matches!(self, Self::Quota | Self::Payment)
    }

    /// The wire spelling, for the surfaces that render a fact in prose (the CLI
    /// status view). Pinned against serde by a test, so the two faces cannot
    /// drift.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Payment => "payment",
            Self::EndpointAuth => "endpoint_auth",
            Self::EndpointShape => "endpoint_shape",
        }
    }
}

/// The endpoint a fact was observed on, as the display face of the endpoint
/// level key. `base_url` is `None` for providers that need none (the native
/// families), which is also what their key component is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EndpointRef {
    pub family: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// One stored fact: the class, the upstream's own words, and when it was seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// Epoch seconds the upstream said the endpoint reopens, when it said so at
    /// all. It can only shorten the readable window, never extend it.
    pub until: Option<u64>,
    /// Bounded excerpt of the upstream's own text (the response body).
    pub detail: String,
    /// The HTTP status the upstream answered, when the failure came from a
    /// response at all. Evidence like `detail` and `until`: stored because the
    /// upstream said it, and never rendered by a reader — it serves one
    /// decision, whether a settled failure tells the gateway this endpoint's
    /// credentials were rejected.
    pub status: Option<u16>,
    /// Epoch seconds when the failure was observed.
    pub recorded_at: u64,
}

/// The one hit predicate: is this fact still readable at `now`?
///
/// Two conditions, both required: the fact is inside its own window
/// (`FACT_TTL_MAX_SECS` from `recorded_at`), and — when the upstream gave a
/// reset — that moment is still ahead. An upstream hint therefore shortens the
/// window when it is nearer, and is ignored when it is further out.
pub fn is_active(recorded_at: u64, until: Option<u64>, now: u64) -> bool {
    if now.saturating_sub(recorded_at) >= FACT_TTL_MAX_SECS {
        return false;
    }
    until.is_none_or(|u| u > now)
}

/// The reset hint as a reader may show it: `None` when the upstream gave none,
/// and `None` when what it gave is past the trust bound (shown as unknown
/// rather than as a promise nobody believes).
pub fn display_until(recorded_at: u64, until: Option<u64>) -> Option<u64> {
    let until = until?;
    let trusted = recorded_at.saturating_add(QUOTA_RESET_TRUST_MAX_SECS);
    (until <= trusted).then_some(until)
}

/// One fact as a status surface shows it: the class, the upstream's words, and
/// an 8-hex hint of the account — never the credential, never the full
/// fingerprint, which stay inside the tagma.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactView {
    pub kind: BlockKind,
    /// Display face of the reset hint; see [`display_until`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    pub detail: String,
    pub recorded_at: u64,
    /// The endpoint level this fact belongs to; `None` for an account-level
    /// fact, which is not about one endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<EndpointRef>,
    /// First 8 hex of the credential fingerprint.
    pub account_hint: String,
}

/// One readable fact about a provider, as a reader that keys a verdict on it
/// sees it: the fact itself, plus the 8-hex account hint the log lines carry.
///
/// The hint is the same face the status surfaces show — never the credential,
/// never the full fingerprint, both of which stay inside the tagma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedFact {
    pub block: Block,
    /// First 8 hex of the credential fingerprint.
    pub account_hint: String,
}

/// The shared fact table, as the adk sees it: hand over a provider id, get
/// either a stored fact back or nothing. Implemented by the tagma, which owns
/// the key computation (the caller never names a credential or a fingerprint).
///
/// Every method is fail-open by contract: a reader that cannot reach the table
/// behaves as if there were no facts, because a missing fact must never be able
/// to change a verdict on its own.
pub trait EndpointHealth: Send + Sync {
    /// Record a failure fact about the endpoint this provider id names.
    fn record(&self, provider_id: &str, block: Block);

    /// The readable fact for this provider id at `now`, if any, with the account
    /// hint a log line carries.
    ///
    /// An account-level fact answers for **every** endpoint on the credential,
    /// not just the one it was recorded through: that is what makes a sibling on
    /// the same account no candidate for escaping an account-level fact, while a
    /// sibling stays a candidate for escaping an endpoint-level one. When both
    /// levels carry a fact, the account-level one answers: the stronger
    /// constraint is the one a reader has to see.
    fn blocked_for(&self, provider_id: &str, now: u64) -> Option<BlockedFact>;

    /// Every readable fact at `now`, for the status and fleet surfaces.
    fn snapshot(&self, now: u64) -> Vec<FactView>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000;

    #[test]
    fn without_a_reset_the_ttl_is_the_whole_window() {
        assert!(is_active(T0, None, T0));
        assert!(is_active(T0, None, T0 + FACT_TTL_MAX_SECS - 1));
        assert!(!is_active(T0, None, T0 + FACT_TTL_MAX_SECS));
        assert!(!is_active(T0, None, T0 + FACT_TTL_MAX_SECS * 10));
    }

    #[test]
    fn a_reset_hint_only_shortens_the_window() {
        // Nearer than the TTL: the hint is the window.
        let near = T0 + 60;
        assert!(is_active(T0, Some(near), near - 1));
        assert!(!is_active(T0, Some(near), near));

        // Further out than the TTL: the TTL still ends it.
        let far = T0 + QUOTA_RESET_TRUST_MAX_SECS * 30;
        assert!(is_active(T0, Some(far), T0 + FACT_TTL_MAX_SECS - 1));
        assert!(!is_active(T0, Some(far), T0 + FACT_TTL_MAX_SECS));
    }

    #[test]
    fn a_reset_beyond_the_trust_bound_displays_as_unknown() {
        let soon = T0 + 600;
        assert_eq!(display_until(T0, Some(soon)), Some(soon));
        assert_eq!(display_until(T0, None), None);

        let too_far = T0 + QUOTA_RESET_TRUST_MAX_SECS + 1;
        assert_eq!(display_until(T0, Some(too_far)), None);
        let at_bound = T0 + QUOTA_RESET_TRUST_MAX_SECS;
        assert_eq!(display_until(T0, Some(at_bound)), Some(at_bound));
    }

    #[test]
    fn only_quota_and_payment_follow_the_account() {
        assert!(BlockKind::Quota.is_account_level());
        assert!(BlockKind::Payment.is_account_level());
        assert!(!BlockKind::EndpointAuth.is_account_level());
        assert!(!BlockKind::EndpointShape.is_account_level());
    }

    #[test]
    fn a_view_serializes_without_the_parts_it_does_not_have() {
        let view = FactView {
            kind: BlockKind::EndpointAuth,
            until: None,
            detail: "HTTP 401".into(),
            recorded_at: T0,
            endpoint: Some(EndpointRef {
                family: "openai".into(),
                base_url: None,
            }),
            account_hint: "0123456789abcdef"[..8].to_string(),
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"endpoint_auth\""));
        assert!(!json.contains("until"));
        assert!(!json.contains("base_url"));
    }

    #[test]
    fn the_kind_spelling_matches_serde() {
        for kind in [
            BlockKind::Quota,
            BlockKind::Payment,
            BlockKind::EndpointAuth,
            BlockKind::EndpointShape,
        ] {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire.as_str(), Some(kind.as_str()), "{kind:?}");
        }
    }
}
