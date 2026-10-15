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

/// Why a gateway-side forward failed before any upstream was reached:
/// the `reason` field of the terminal failure line. The category is the
/// machine-filterable half of the attribution, while the line's `error`
/// field carries the readable text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InternalFailureReason {
    /// A store read behind the selection failed.
    StoreError,
    /// The served profile has no provider credential row.
    MissingCredential,
    /// The credential's endpoint prefix does not parse into a dial target.
    UnparseableEndpointPrefix,
}

impl InternalFailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StoreError => "store_error",
            Self::MissingCredential => "missing_credential",
            Self::UnparseableEndpointPrefix => "unparseable_endpoint_prefix",
        }
    }
}

pub struct GatewayCtx {
    pub route: Route,
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
    /// Why a gateway-side internal failure happened, staged with the
    /// pending outcome. Invariant: it is set only for an `internal_error`
    /// line -- never for a gate refusal or a wire failure.
    pub pending_reason: Option<InternalFailureReason>,
    /// The gateway-side failure's own message, staged with the reason and
    /// rendered in the line's `error` field. Same invariant: only the
    /// `internal_error` line carries it.
    pub pending_detail: Option<String>,
    /// The upstream status line staged at the first response
    /// header, for the terminal classification.
    pub upstream_status: Option<u16>,
    /// Whether the upstream connection was established or reused
    /// for this request: the discriminator between a connect
    /// error and an upstream error at the terminal phase.
    pub connected: bool,
}
