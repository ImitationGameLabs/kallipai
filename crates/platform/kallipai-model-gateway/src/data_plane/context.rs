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
}

pub struct GatewayCtx {
    pub route: Route,
    pub selection: Option<Selection>,
    /// The forwarding clock: `t0` at the Forward branch entry,
    /// `t1` at the first upstream response header. The duration
    /// metrics observe them at the terminal phase.
    pub t0: Option<std::time::Instant>,
    pub t1: Option<std::time::Instant>,
    /// The outcome a gate staged in `request_filter`; the `logging`
    /// phase is the single terminal record point and prefers it
    /// over anything derived.
    pub pending_outcome: Option<crate::metrics::ForwardOutcome>,
    /// The upstream status line staged at the first response
    /// header, for the terminal classification.
    pub upstream_status: Option<u16>,
    /// Whether the upstream connection was established or reused
    /// for this request: the discriminator between a connect
    /// error and a proxy error at the terminal phase.
    pub connected: bool,
}
