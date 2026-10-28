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

/// How long one dial may spend on a single connect step. The TCP
/// connect and the TLS handshake that follows it each get this much
/// on their own: pingora-core's connectors/l4.rs:158 wraps the
/// connect, connectors/tls/rustls/mod.rs:284 the handshake. Both
/// are capped by whatever the remaining budget leaves, and the
/// pair together answers to that same remaining budget as a whole
/// (connectors/mod.rs:403).
/// Three seconds because Linux retransmits a lost SYN at one second
/// and at three: a candidate that needs the second retransmit is cut
/// there, before its ACK can arrive. That schedule sets the
/// ceiling; the handshake reads the same field rather than a value
/// of its own, and three seconds sits well above what it needs.
/// The rest of the budget stays unspent on a failed dial: the
/// next candidate gets what is left, so the ceiling is one
/// address's cost rather than the whole request's. Fixed
/// rather than configurable until an operator asks for a knob.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// How much wall time one request may keep walking candidates: the
/// budget closes the cursor's gate and, through the remaining time,
/// bounds each dial -- one dial's connect step and its handshake.
/// Counted from the moment the selection is built, so the resolver
/// answer that came before it is not part of the budget. Fixed
/// rather than configurable until an operator asks for a knob.
const DIAL_BUDGET: Duration = Duration::from_secs(8);

/// The candidate cursor: which address the next dial takes. The gate
/// opens while candidates remain and the dial budget is unspent; once
/// it closes the cursor hands out nothing, so a caller never has to
/// decide what an exhausted list dials.
///
/// `next` is the candidate an open gate would hand out; before any
/// dial it is 0, and a closed gate never consults it.
///
/// `candidate_count` is at least one -- `resolve_candidates` refuses
/// an empty answer -- so the cap is the only thing that can hide an
/// address.
pub(crate) struct Attempts {
    next: usize,
    limit: usize,
    deadline: Instant,
}

impl Attempts {
    /// A cursor over `candidate_count` candidates, capped at
    /// `MAX_CANDIDATES`, whose gate closes at `deadline`.
    pub(crate) fn new(candidate_count: usize, deadline: Instant) -> Self {
        Self {
            next: 0,
            limit: candidate_count.min(MAX_CANDIDATES),
            deadline,
        }
    }

    /// Whether the gate is still open: candidates remain and the dial
    /// budget is unspent. A caller that wants to know before it asks
    /// for an address reads this, not the cursor's insides.
    pub(crate) fn can_retry(&self) -> bool {
        self.next < self.limit && Instant::now() < self.deadline
    }

    /// The address index the next dial takes, or nothing once the gate
    /// has closed.
    pub(crate) fn take(&mut self) -> Option<usize> {
        if !self.can_retry() {
            return None;
        }
        let index = self.next;
        self.next += 1;
        Some(index)
    }

    /// The dial budget left: how long a dial starting now may take. It
    /// feeds the dial timeouts only -- whether another candidate is
    /// worth trying is the gate's question, and the gate reads the
    /// deadline itself.
    pub(crate) fn remaining_budget(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
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
    ///
    /// The timeouts come from the dial budget's remaining time: this
    /// dial's connect step gets `CONNECT_TIMEOUT`, capped by what the
    /// budget leaves, and the whole L4-plus-TLS sequence gets the
    /// remaining time outright.
    pub(crate) fn peer_at(&self, index: usize) -> HttpPeer {
        let mut peer = HttpPeer::new(self.addresses[index], self.dial.tls, self.dial.sni.clone());
        let remaining = self.attempts.remaining_budget();
        peer.options.connection_timeout = Some(CONNECT_TIMEOUT.min(remaining));
        peer.options.total_connection_timeout = Some(remaining);
        peer
    }

    /// Whether another candidate is worth a dial: the attempt cursor's
    /// gate, asked without asking for an address.
    pub(crate) fn can_retry(&self) -> bool {
        self.attempts.can_retry()
    }

    /// The peer for the next dial: the following candidate in resolver
    /// order while the candidate list and the dial budget last, and
    /// nothing once either runs out.
    pub(crate) fn next_peer(&mut self) -> Option<HttpPeer> {
        let index = self.attempts.take()?;
        Some(self.peer_at(index))
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
    /// Whether the upstream connection of the dial that ended this
    /// request was established or reused: the discriminator between a
    /// connect error and an upstream error at the terminal phase.
    /// `upstream_peer` clears it before every dial, so a retry leaves
    /// the last dial's fact here, not the request's first connection.
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

    /// A dial's peer carries the timeouts the budget buys: the connect
    /// step gets `CONNECT_TIMEOUT` while the budget is the longer of the
    /// two, the whole L4-plus-TLS sequence gets the remaining budget, and
    /// a budget shorter than `CONNECT_TIMEOUT` caps the connect step as
    /// well. A spent budget hands out no peer at all: the gate closes
    /// before a dial is built, so there is no zero-timeout peer to use.
    ///
    /// The upstream H1 and H2 client sessions copy the peer's read and
    /// write timeouts onto the stream (pingora-proxy's `proxy_1to1` at
    /// `proxy_h1.rs:43-44`; H2 at `proxy_h2.rs:271` and `:293`), so a
    /// value there is enforced. Both stay unset because this bound
    /// covers getting connected, and because a per-read bound on the
    /// response cannot tell a quiet stream from a stalled one. The
    /// response side carries no bound of its own today either, so
    /// that call stays open rather than taken.
    #[test]
    fn a_peer_carries_the_dial_timeouts() {
        let mut selection = selection_with(DialParams {
            tls: false,
            sni: String::new(),
        });
        let peer = selection
            .next_peer()
            .expect("the first candidate is in budget");
        assert_eq!(peer.options.connection_timeout, Some(CONNECT_TIMEOUT));
        assert!(peer.options.read_timeout.is_none());
        assert!(peer.options.write_timeout.is_none());
        let total = peer.options.total_connection_timeout.unwrap_or_default();
        assert!(total <= DIAL_BUDGET);
        assert!(total > DIAL_BUDGET - Duration::from_secs(1));
        selection.attempts = Attempts::new(2, Instant::now() + Duration::from_secs(2));
        let close = selection
            .next_peer()
            .expect("a two-second budget still dials");
        let capped = close.options.connection_timeout.unwrap_or_default();
        assert_eq!(
            capped,
            close.options.total_connection_timeout.unwrap_or_default()
        );
        assert!(capped < CONNECT_TIMEOUT);
        assert!(capped > Duration::from_secs(1));
        selection.attempts = Attempts::new(2, Instant::now());
        assert!(selection.next_peer().is_none());
    }

    /// The dial bound is a premise, not a tuning value: three seconds is
    /// where the kernel's second SYN retransmit lands, so an edit there
    /// moves the ceiling every dial reads -- this test makes that edit
    /// loud.
    #[test]
    fn the_connect_timeout_matches_the_syn_retransmit_schedule() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(3));
    }

    /// The cursor walks the candidates in the order the resolver
    /// answered, then closes.
    #[test]
    fn attempts_follow_the_resolver_order() {
        let mut attempts = Attempts::starting_now(3);
        let served: Vec<Option<usize>> = (0..4).map(|_| attempts.take()).collect();
        assert_eq!(served, vec![Some(0), Some(1), Some(2), None]);
    }

    /// A spent budget closes the gate before the first dial: the cursor
    /// hands out nothing, while a fresh cursor still starts at the first
    /// candidate.
    #[test]
    fn the_budget_stops_the_attempts() {
        let spent_at = Instant::now() - Duration::from_secs(1);
        let mut spent = Attempts::new(3, spent_at);
        assert!(spent.take().is_none());
        assert!(spent.take().is_none());
        let mut fresh = Attempts::starting_now(3);
        assert_eq!(fresh.take(), Some(0));
        assert_eq!(fresh.take(), Some(1));
    }

    /// A budget that runs out after the cursor moved closes the gate
    /// where the cursor stands: what it has behind it no longer buys a
    /// dial.
    #[test]
    fn the_budget_closing_midway_hands_out_nothing() {
        let mut attempts = Attempts {
            next: 2,
            limit: 3,
            deadline: Instant::now() - Duration::from_secs(1),
        };
        assert!(attempts.take().is_none());
        assert!(attempts.take().is_none());
    }

    /// Nine candidates still buy eight dials: the cap closes the gate
    /// exactly as the end of the list does.
    #[test]
    fn the_candidate_cap_bounds_the_attempts() {
        let mut attempts = Attempts::starting_now(9);
        let served: Vec<Option<usize>> = (0..9).map(|_| attempts.take()).collect();
        assert_eq!(
            served,
            vec![
                Some(0),
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                None
            ]
        );
    }

    /// The dial follows the cursor: the second dial takes the second
    /// candidate, and once the list is spent there is no dial left to
    /// make.
    #[test]
    fn next_peer_follows_the_cursor_over_the_candidates() {
        let mut selection = selection_with(DialParams {
            tls: false,
            sni: "upstream.test".to_string(),
        });
        let first = selection
            .next_peer()
            .expect("the first candidate is dialled");
        let second = selection
            .next_peer()
            .expect("the second candidate is dialled");
        let third = selection.next_peer();
        assert_eq!(
            first._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 1], 8443)))
        );
        assert_eq!(
            second._address.as_inet(),
            Some(&SocketAddr::from(([192, 0, 2, 2], 8443)))
        );
        assert!(third.is_none());
    }

    /// A gate that closed before the first dial hands out no peer: the
    /// cursor is the only thing that answers whether a dial is left.
    #[test]
    fn a_closed_gate_hands_out_no_peer() {
        let mut selection = selection_with(DialParams {
            tls: false,
            sni: String::new(),
        });
        selection.attempts = Attempts::new(2, Instant::now() - Duration::from_secs(1));
        assert!(!selection.can_retry());
        assert!(selection.next_peer().is_none());
    }
}
