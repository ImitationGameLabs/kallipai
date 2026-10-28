//! The per-request context: what the phases hand to each other.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use pingora::prelude::HttpPeer;
use tokio::time::Instant;

use crate::forward::dialect;
use crate::secret::ProviderCredential;

/// Which face the request belongs to, decided once in `request_filter`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route {
    /// One of the three generation endpoints (the path names which).
    Forward(dialect::Endpoint),
    /// The process health probe.
    Health,
    /// The management family: never served here (the axum face owns it).
    Admin,
    /// Anything else: 404.
    NotFound,
    /// A known data-plane path with the wrong method: 405.
    MethodNotAllowed,
}

impl Route {
    /// The route label a request span carries as `http.route`: a
    /// cardinality-bounded name, never the concrete path. The entry
    /// paths are these, not the wire paths an `Endpoint` names.
    ///
    /// A CORS preflight is not one of these: an OPTIONS probe at a path whose
    /// method is POST would key the 405, so `request_filter` covers it apart
    /// and names the span `preflight` instead of asking for a label here.
    pub fn label(self) -> &'static str {
        match self {
            Self::Forward(dialect::Endpoint::ChatCompletions) => "/chat/completions",
            Self::Forward(dialect::Endpoint::Responses) => "/responses",
            Self::Forward(dialect::Endpoint::Messages) => "/messages",
            Self::Health => "/health",
            Self::Admin => "/admin/*",
            Self::NotFound => "unmatched",
            Self::MethodNotAllowed => "method_not_allowed",
        }
    }
}

/// The selection outcome: the profile the request rides and the pieces
/// the later phases need: the upstream candidates and the dial
/// parameters, the credential, and the rewrite inputs (the base path
/// and the authority for the Host header).
/// The candidate cursor travels with the candidate list: the peer
/// phase advances it, so a second ask walks to the next candidate.
pub struct Selection {
    /// The upstream candidates in resolver-answer order, each already
    /// carrying the credential prefix's port: the port is not part of
    /// the cache key, so it is joined here once.
    pub addresses: Arc<[SocketAddr]>,
    /// The parameters a peer needs besides its address: kept so any
    /// candidate builds a dial target shaped like the first try.
    pub dial: DialParams,
    /// The candidate cursor: which address the next dial takes. Built
    /// at the selection point, so the dial budget counts from there.
    pub attempts: Attempts,
    pub credential: ProviderCredential,
    pub auth: crate::secret::AuthStyle,
    /// The registry family of the selected provider: the `api_family`
    /// attribution label.
    pub api_family: String,
    /// The credential's base path, trimmed of its trailing slash; the
    /// endpoint's wire path appends to it in the rewrite phase.
    pub base_path: String,
    /// The upstream URL authority verbatim (the Host header value).
    pub authority: String,
    /// The profile the request rides: the terminal failure line
    /// names it, so a failure is attributable to a profile.
    pub profile_id: String,
}

/// The peer parameters that are not the address: whether the
/// connection is TLS and the name the certificate must carry.
pub(crate) struct DialParams {
    pub tls: bool,
    pub sni: String,
}

/// How many candidates one request may try: a defence against a
/// pathological answer rather than a tuning knob -- a normal answer
/// carries a handful. Fixed rather than configurable until an operator
/// asks for a knob.
const MAX_CANDIDATES: usize = 8;

/// How much wall time one request may keep walking candidates: the
/// budget closes the cursor's gate, it does not bound a single dial --
/// no dial has a timeout of its own. Counted from the moment the
/// selection is built. Fixed rather than configurable until an
/// operator asks for a knob.
const DIAL_BUDGET: Duration = Duration::from_secs(8);

/// The candidate cursor: which address the next dial takes. The gate
/// opens while candidates remain and the dial budget is unspent; once
/// it closes, the cursor keeps handing back the address it gave out
/// last, so a caller never has to decide what an exhausted list dials.
///
/// `next` is the candidate an open gate would hand out and `last` the
/// one handed out most recently, so `last` sits one behind it; before
/// any dial both are 0, which is what lets a gate that is already
/// closed still start at the first candidate.
///
/// `candidate_count` is at least one -- `resolve_candidates` refuses an
/// empty answer -- so the cursor can always hand out an index.
pub(crate) struct Attempts {
    next: usize,
    last: usize,
    limit: usize,
    deadline: Instant,
}

impl Attempts {
    /// A cursor over `candidate_count` candidates, capped at
    /// `MAX_CANDIDATES`, whose gate closes at `deadline`.
    pub(crate) fn new(candidate_count: usize, deadline: Instant) -> Self {
        Self {
            next: 0,
            last: 0,
            limit: candidate_count.min(MAX_CANDIDATES),
            deadline,
        }
    }

    /// The address index the next dial takes: the following candidate
    /// while the gate is open, and the last one handed out once it
    /// closes.
    pub(crate) fn next_index(&mut self) -> usize {
        if self.next < self.limit && Instant::now() < self.deadline {
            self.last = self.next;
            self.next += 1;
        }
        self.last
    }

    /// A cursor whose budget runs out `DIAL_BUDGET` from now: the
    /// shape the gateway builds once a selection is resolved.
    pub(crate) fn starting_now(candidate_count: usize) -> Self {
        Self::new(candidate_count, Instant::now() + DIAL_BUDGET)
    }
}

impl Selection {
    /// The peer for one candidate: the address at `index` and the dial
    /// parameters every candidate shares.
    ///
    /// `addresses` is never empty -- `resolve_candidates` refuses an empty
    /// answer -- so a caller only has to pass an index inside it.
    pub(crate) fn peer_at(&self, index: usize) -> HttpPeer {
        HttpPeer::new(self.addresses[index], self.dial.tls, self.dial.sni.clone())
    }

    /// The peer for the next dial: the following candidate in resolver
    /// order while the candidate list and the dial budget last, and the
    /// last candidate tried once either runs out.
    pub(crate) fn next_peer(&mut self) -> HttpPeer {
        let index = self.attempts.next_index();
        self.peer_at(index)
    }
}

/// Why a forward failed before any upstream answered: the value domain
/// of the failure line's `reason` field, and the machine-filterable
/// half of the attribution -- the line's `error` field carries the
/// readable text. The counters keep their own table under
/// `error_class`, so the two vocabularies never share a value set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureReason {
    /// A store read behind the selection failed.
    StoreError,
    /// The served profile has no provider credential row.
    MissingCredential,
    /// The credential's endpoint prefix does not parse into a dial target.
    UnparseableEndpointPrefix,
    /// The endpoint's host did not resolve: the resolver failed,
    /// answered nothing, or overran the gateway's bound.
    UnresolvedUpstream,
}

impl FailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StoreError => "store_error",
            Self::MissingCredential => "missing_credential",
            Self::UnparseableEndpointPrefix => "unparseable_endpoint_prefix",
            Self::UnresolvedUpstream => "unresolved_upstream",
        }
    }
}

pub struct GatewayCtx {
    pub route: Route,
    /// The span the request runs inside, opened at the top of
    /// `request_filter`: every later phase is instrumented with it,
    /// so a call this gateway makes lands under the request that
    /// caused it instead of opening a trace of its own.
    pub span: Option<tracing::Span>,
    pub selection: Option<Selection>,
    /// The authenticated tagma, staged at the identity gate: the
    /// terminal failure line names the caller behind a failure.
    pub tagma_id: Option<String>,
    /// The forwarding clock: `t0` at the Forward branch entry,
    /// `t1` at the first upstream response header. The duration
    /// metrics observe them at the terminal phase.
    pub t0: Option<std::time::Instant>,
    pub t1: Option<std::time::Instant>,
    /// The outcome a gate staged in `request_filter`; the `logging`
    /// phase is the single terminal record point and prefers it
    /// over anything derived.
    pub pending_outcome: Option<crate::metrics::ForwardOutcome>,
    /// Why a failure line carries a reason: staged with the outcome
    /// the gateway staged. Invariant: it rides only a line the gateway
    /// answers itself -- `internal_error`, or `resolve_error` before an
    /// upstream was ever reached -- never a gate refusal or a wire
    /// failure.
    ///
    /// The invariant runs one way. The forward phase that runs without
    /// its staged selection is an `internal_error` line with no
    /// category, because no input caused it, and that line reaches the
    /// `logging` phase like any other forward. The rewrite phase on a
    /// route `request_filter` never forwards stages nothing: it returns
    /// a pingora internal error, and that route never enters the
    /// forwarding face the `logging` phase records, so no line is
    /// written for it.
    pub pending_reason: Option<FailureReason>,
    /// The failure's own message, staged with the reason and rendered
    /// in the line's `error` field. Same invariant: only a line the
    /// gateway answers itself carries it.
    pub pending_detail: Option<String>,
    /// The upstream status line staged at the first response
    /// header, for the terminal classification.
    pub upstream_status: Option<u16>,
    /// Whether the upstream connection was established or reused
    /// for this request: the discriminator between a connect
    /// error and an upstream error at the terminal phase.
    pub connected: bool,
}

impl GatewayCtx {
    /// The request span, or a disabled one: instrumenting a phase with it
    /// does nothing.
    ///
    /// The invariant behind it: `request_filter` is the first phase of a
    /// request and stores the span before anything answers, so every later
    /// phase finds one. The fallback hands out a disabled span rather than
    /// panicking, which leaves the invariant a statement the code makes
    /// rather than one an `expect` enforces -- and no caller in the tree
    /// walks in without it today.
    pub fn phase_span(&self) -> tracing::Span {
        self.span.clone().unwrap_or_else(tracing::Span::none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A selection over two candidate addresses with the given dial
    /// parameters: everything else is fixed, so a test varies only the
    /// part it reads.
    fn selection_with(dial: DialParams) -> Selection {
        Selection {
            addresses: Arc::from(vec![
                SocketAddr::from(([192, 0, 2, 1], 8443)),
                SocketAddr::from(([192, 0, 2, 2], 8443)),
            ]),
            dial,
            attempts: Attempts::starting_now(2),
            credential: ProviderCredential::placeholder(),
            auth: crate::secret::AuthStyle::Bearer,
            api_family: "openai".to_string(),
            base_path: "/v1".to_string(),
            authority: "upstream.test:8443".to_string(),
            profile_id: "profile-a".to_string(),
        }
    }

    /// Each candidate's peer carries that candidate's own address and the
    /// dial parameters every candidate shares. The secure call keeps its
    /// SNI different from the authority, so its assertions say where the
    /// value came from; the TLS flag is read both ways round.
    #[test]
    fn peer_at_builds_the_dial_target_for_a_candidate_at_its_index() {
        let selection = selection_with(DialParams {
            tls: false,
            sni: "upstream.test".to_string(),
        });
        let peer = selection.peer_at(0);
        assert_eq!(
            peer._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 1], 8443)))
        );
        assert_eq!(peer.sni, "upstream.test");
        assert!(!peer.is_tls());
        let second = selection.peer_at(1);
        assert_eq!(
            second._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 2], 8443)))
        );
        assert_eq!(second.sni, "upstream.test");
        assert!(!second.is_tls());
        let secure = selection_with(DialParams {
            tls: true,
            sni: "secure.test".to_string(),
        });
        assert!(secure.peer_at(0).is_tls());
        assert!(secure.peer_at(1).is_tls());
        assert_eq!(secure.peer_at(0).sni, "secure.test");
        assert_eq!(secure.peer_at(1).sni, "secure.test");
    }

    /// The cursor walks the candidates in the order the resolver
    /// answered, then keeps handing back the last one it gave out.
    #[test]
    fn attempts_follow_the_resolver_order() {
        let mut attempts = Attempts::starting_now(3);
        let served: Vec<usize> = (0..4).map(|_| attempts.next_index()).collect();
        assert_eq!(served, vec![0, 1, 2, 2]);
    }

    /// A spent budget closes the gate before the first dial: the cursor
    /// hands out the first candidate rather than jumping to the last,
    /// and it does not advance afterwards.
    #[test]
    fn the_budget_stops_the_attempts() {
        let spent_at = Instant::now() - Duration::from_secs(1);
        let mut spent = Attempts::new(3, spent_at);
        assert_eq!(spent.next_index(), 0);
        assert_eq!(spent.next_index(), 0);
        let mut fresh = Attempts::starting_now(3);
        assert_eq!(fresh.next_index(), 0);
        assert_eq!(fresh.next_index(), 1);
    }

    /// A budget that runs out after the cursor moved keeps the cursor
    /// where it stands: the gate closes onto the index handed out last,
    /// not onto the first candidate and not onto the end of the list.
    #[test]
    fn the_budget_closing_midway_repeats_the_last_index() {
        let mut attempts = Attempts {
            next: 2,
            last: 1,
            limit: 3,
            deadline: Instant::now() - Duration::from_secs(1),
        };
        assert_eq!(attempts.next_index(), 1);
        assert_eq!(attempts.next_index(), 1);
    }

    /// Nine candidates still buy eight dials: the cap closes the gate
    /// exactly as the end of the list does.
    #[test]
    fn the_candidate_cap_bounds_the_attempts() {
        let mut attempts = Attempts::starting_now(9);
        let served: Vec<usize> = (0..9).map(|_| attempts.next_index()).collect();
        assert_eq!(served, vec![0, 1, 2, 3, 4, 5, 6, 7, 7]);
    }

    /// The dial follows the cursor: the second dial takes the second
    /// candidate, and once the list is spent the dials keep taking the
    /// last one.
    #[test]
    fn next_peer_follows_the_cursor_over_the_candidates() {
        let mut selection = selection_with(DialParams {
            tls: false,
            sni: "upstream.test".to_string(),
        });
        let first = selection.next_peer();
        let second = selection.next_peer();
        let third = selection.next_peer();
        assert_eq!(
            first._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 1], 8443)))
        );
        assert_eq!(
            second._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 2], 8443)))
        );
        assert_eq!(
            third._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 2], 8443)))
        );
    }
}
