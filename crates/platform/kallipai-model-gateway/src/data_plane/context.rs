//! The per-request context: what the phases hand to each other.

use pingora::prelude::HttpPeer;

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
/// the later phases need: the peer, the credential, and the rewrite
/// inputs (the base path and the authority for the Host header).
pub struct Selection {
    pub peer: HttpPeer,
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
