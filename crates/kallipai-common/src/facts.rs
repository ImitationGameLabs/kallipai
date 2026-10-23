//! Shared upstream-failure facts: what one agent learned about a profile,
//! readable by every agent.
//!
//! A fact is written when a failure verdict says the profile cannot serve the
//! request (quota, payment, credentials, a repeated response shape). The table
//! lives in the tagma and is keyed by the profile id — the unit an agent runs,
//! and the unit an upstream answers about, so the id a writer hands its failure
//! in under is the id a reader gets its verdict back from.
//!
//! This module carries the value types and the one window rule every reader
//! shares. The implementation (storage, capacity, claiming) is the tagma's; the
//! adk only holds the handle.

use serde::{Deserialize, Serialize};

/// The window a fact leaves behind when the upstream named no reset moment.
///
/// Short by intent: it buys one probing send per window rather than an hour of
/// silence, so a limit the upstream never explained costs a pause instead of a
/// parked round.
pub const MIN_RETRY_INTERVAL_SECS: u64 = 60;

/// The same window for a payment wall, which does not clear on its own within
/// seconds: a longer pause is cheaper than asking a billing gate twice.
pub const PAYMENT_RETRY_INTERVAL_SECS: u64 = 300;

/// How many facts the table keeps. Oldest by `recorded_at` is evicted when a
/// new key arrives at capacity, so a long-lived tagma cannot grow without bound.
pub const FACT_TABLE_CAPACITY: usize = 256;

/// Trust bound on an upstream reset hint: a moment further out than this is
/// shown as unknown, and [`window_end`] caps the window there too, so what a
/// surface shows and what a reader honors cannot disagree.
pub const QUOTA_RESET_TRUST_MAX_SECS: u64 = 86_400;

/// What the upstream rejected.
///
/// The kinds name where the wall is, not who owns it: every fact is recorded
/// against one profile, and none of them reaches a sibling. They differ in how
/// the round ends ([`BlockKind::parks_the_round`]) and in how long the record
/// stays readable ([`retry_interval`]).
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
    /// Whether a fact of this kind ends the round on the spot: the quota and
    /// payment exits carry the structured parked reason, where the others stop
    /// with the failure that caused them.
    pub fn parks_the_round(self) -> bool {
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

/// The window a fact of this kind leaves behind when the upstream said nothing
/// about when the profile may be used again.
pub fn retry_interval(kind: BlockKind) -> u64 {
    match kind {
        BlockKind::Payment => PAYMENT_RETRY_INTERVAL_SECS,
        BlockKind::Quota | BlockKind::EndpointAuth | BlockKind::EndpointShape => {
            MIN_RETRY_INTERVAL_SECS
        }
    }
}

/// One stored fact: the class, the upstream's own words, and when the window it
/// opens starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// Epoch seconds the upstream said the endpoint reopens, when it said so at
    /// all. It can only move the window earlier, never past the trust bound.
    pub until: Option<u64>,
    /// Bounded excerpt of the upstream's own text (the response body).
    pub detail: String,
    /// The HTTP status the upstream answered, when the failure came from a
    /// response at all. Evidence like `detail` and `until`: stored because the
    /// upstream said it, and never rendered by a reader — it serves one
    /// decision, whether a settled failure tells the gateway this endpoint's
    /// credentials were rejected.
    pub status: Option<u16>,
    /// Epoch seconds when the window this fact opens begins.
    ///
    /// One moment, one meaning: it is the writer's clock at the failure, and
    /// the only time a reader uses, so the write side and the read side cannot
    /// disagree about when the window started.
    pub recorded_at: u64,
}

/// The moment this fact stops being readable: the end of the window it opened.
///
/// An upstream reset ends it when that is nearer, capped by the trust bound so
/// a hint nobody should believe cannot pin a profile for a day. Without a hint
/// the window is the kind's own interval ([`retry_interval`]).
pub fn window_end(block: &Block) -> u64 {
    let capped = block.recorded_at.saturating_add(QUOTA_RESET_TRUST_MAX_SECS);
    match block.until {
        Some(until) => until.min(capped),
        None => block.recorded_at.saturating_add(retry_interval(block.kind)),
    }
}

/// The one hit predicate: is this fact still readable at `now`?
///
/// A fact is readable until its window ends, and no longer: every reader asks
/// this one question, so an expired fact is never half there.
pub fn is_active(block: &Block, now: u64) -> bool {
    now < window_end(block)
}

/// The reset hint as a reader may show it: `None` when the upstream gave none,
/// and `None` when what it gave is past the trust bound (shown as unknown
/// rather than as a promise nobody believes).
pub fn display_until(recorded_at: u64, until: Option<u64>) -> Option<u64> {
    let until = until?;
    let trusted = recorded_at.saturating_add(QUOTA_RESET_TRUST_MAX_SECS);
    (until <= trusted).then_some(until)
}

/// One fact as a status surface shows it: the kind, the upstream's words, and
/// the profile it names — never a credential, which stays inside the tagma.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactView {
    pub kind: BlockKind,
    /// Display face of the reset hint; see [`display_until`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    pub detail: String,
    pub recorded_at: u64,
    /// The profile the fact names.
    pub profile: String,
}

/// One readable fact about a profile, as a reader that keys a verdict on it sees
/// it: the fact itself, plus the profile id the log lines carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedFact {
    pub block: Block,
    /// The profile the fact names.
    pub profile: String,
}

/// The shared fact table, as the adk sees it: hand over a profile id, get
/// either a stored fact back or nothing. Implemented by the tagma, which owns
/// the storage.
///
/// Every method is fail-open by contract: a reader that cannot reach the table
/// behaves as if there were no facts, because a missing fact must never be able
/// to change a verdict on its own.
pub trait EndpointHealth: Send + Sync {
    /// Record a failure fact about this profile.
    fn record(&self, profile_id: &str, block: Block);

    /// The readable fact for this profile at `now`, if any.
    ///
    /// A fact answers for the profile it was recorded on and for no other: a
    /// sibling profile is a candidate again the moment the walk reaches it, and
    /// pays its own probing send to find out.
    fn blocked_for(&self, profile_id: &str, now: u64) -> Option<BlockedFact>;

    /// Every readable fact at `now`, for the status and fleet surfaces.
    fn snapshot(&self, now: u64) -> Vec<FactView>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000;

    fn block(kind: BlockKind, until: Option<u64>) -> Block {
        Block {
            kind,
            until,
            detail: "HTTP 429".into(),
            status: None,
            recorded_at: T0,
        }
    }

    #[test]
    fn without_a_reset_the_window_is_the_kind_interval() {
        let quota = block(BlockKind::Quota, None);
        assert!(is_active(&quota, T0));
        assert!(is_active(&quota, T0 + MIN_RETRY_INTERVAL_SECS - 1));
        assert!(!is_active(&quota, T0 + MIN_RETRY_INTERVAL_SECS));
        assert_eq!(window_end(&quota), T0 + MIN_RETRY_INTERVAL_SECS);

        let payment = block(BlockKind::Payment, None);
        assert!(is_active(&payment, T0 + PAYMENT_RETRY_INTERVAL_SECS - 1));
        assert!(!is_active(&payment, T0 + PAYMENT_RETRY_INTERVAL_SECS));
        assert_eq!(window_end(&payment), T0 + PAYMENT_RETRY_INTERVAL_SECS);

        // The kinds that never park a round wait the plain interval.
        for kind in [BlockKind::EndpointAuth, BlockKind::EndpointShape] {
            assert_eq!(retry_interval(kind), MIN_RETRY_INTERVAL_SECS, "{kind:?}");
        }
    }

    #[test]
    fn a_reset_hint_ends_the_window_when_it_is_nearer() {
        let near = T0 + 30;
        let hinted = block(BlockKind::Quota, Some(near));
        assert_eq!(window_end(&hinted), near);
        assert!(is_active(&hinted, near - 1));
        assert!(!is_active(&hinted, near));
    }

    #[test]
    fn a_reset_beyond_the_trust_bound_is_capped() {
        let far = T0 + QUOTA_RESET_TRUST_MAX_SECS * 30;
        let capped = block(BlockKind::Quota, Some(far));
        assert_eq!(window_end(&capped), T0 + QUOTA_RESET_TRUST_MAX_SECS);
        assert!(is_active(&capped, T0 + QUOTA_RESET_TRUST_MAX_SECS - 1));
        assert!(!is_active(&capped, T0 + QUOTA_RESET_TRUST_MAX_SECS));
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
    fn only_quota_and_payment_park_the_round() {
        assert!(BlockKind::Quota.parks_the_round());
        assert!(BlockKind::Payment.parks_the_round());
        assert!(!BlockKind::EndpointAuth.parks_the_round());
        assert!(!BlockKind::EndpointShape.parks_the_round());
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

    #[test]
    fn a_view_serializes_exactly_the_fields_it_has() {
        let view = FactView {
            kind: BlockKind::EndpointAuth,
            until: None,
            detail: "HTTP 401".into(),
            recorded_at: T0,
            profile: "a".into(),
        };
        let json = serde_json::to_string(&view).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"endpoint_auth","detail":"HTTP 401","recorded_at":1700000000,"profile":"a"}"#
        );
    }
}
