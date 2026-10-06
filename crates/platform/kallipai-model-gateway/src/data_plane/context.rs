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
    /// The credential's base path, trimmed of its trailing slash; the
    /// endpoint's wire path appends to it in the rewrite phase.
    pub base_path: String,
    /// The upstream URL authority verbatim (the Host header value).
    pub authority: String,
}

pub struct GatewayCtx {
    pub route: Route,
    pub selection: Option<Selection>,
}
