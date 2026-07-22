//! The narrow interface the data-plane relay (`kallip-lesche`) uses to talk to
//! the registry (`kallip-agora`). The lesche never touches the durable store
//! directly; it authenticates requests, resolves tagma metadata, and advances
//! the tunnel-proof replay guard through this trait.
//!
//! Keeping the surface small and stable is the point of the control-plane /
//! data-plane split: app↔herald business evolution happens inside the lesche
//! and the shared wire types, never here. The lesche runs as a separate service
//! and reaches this trait over the `/internal/*` HTTP API via an RPC client impl
//! (`HttpControlPlane`); the on-wire contract for that API lives in
//! [`crate::internal_api`].

use crate::bytes::Ed25519PublicKey;
use crate::ids::{TagmaId, UserId};
use crate::principal::Principal;

/// A tagma's registry identity, fetched once at herald-tunnel connect time to
/// both verify the reconnect proof (pinned key) and route presence to the
/// owning user.
#[derive(Debug, Clone)]
pub struct TagmaIdentity {
    /// The Ed25519 public key pinned at enrollment.
    pub pinned_public_key: Ed25519PublicKey,
    /// The user who owns this tagma (receives its presence + envelopes).
    pub owner_user_id: UserId,
}

/// Why a [`ControlPlane`] call failed. Surfaces as HTTP 500 at the relay; the
/// relay maps "not found / unauthorized" outcomes to `Option::None` rather than
/// to errors so they can become precise 404/401s.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("registry backend failure: {0}")]
    Backend(String),
}

/// The registry, as seen by the relay. All methods are `async` (the DB-backed
/// impl awaits; a future RPC impl awaits the network) and are always called
/// *outside* any relay soft-state lock.
#[async_trait::async_trait]
pub trait ControlPlane: Send + Sync + 'static {
    /// Verify a `kallip_session` cookie value -> the owning user, or `None` if
    /// the session is absent / expired / disabled. By construction this can
    /// only ever produce a `User` (the deputy guard: a `User` is reachable ONLY
    /// via the cookie).
    async fn verify_session(&self, cookie_value: &str)
    -> Result<Option<UserId>, ControlPlaneError>;

    /// Verify an `Authorization: Bearer` token -> an `Admin` or `Tagma`
    /// principal, or `None` if invalid / revoked / owner-disabled. (`Admin` is
    /// returned but rejected by the relay's `require_tagma` on data-plane
    /// routes, matching the registry's own behavior.)
    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError>;

    /// Is `tagma_id` owned by `user` AND enrolled? A single boolean so the relay
    /// can surface one existence-oracle 404 for unknown / pending / non-owner
    /// without distinguishing them.
    async fn tagma_resolvable_by(
        &self,
        tagma_id: &TagmaId,
        user: &UserId,
    ) -> Result<bool, ControlPlaneError>;

    /// The tagma's pinned public key + owner, or `None` if the tagma is unknown
    /// / has no pinned key. Backs the herald-tunnel reconnect: the relay
    /// verifies the Ed25519 proof against the pinned key locally, and uses the
    /// owner to route presence.
    async fn tagma_identity(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<TagmaIdentity>, ControlPlaneError>;

    /// Atomically advance the tagma's tunnel-proof replay high-water-mark to
    /// `ts`. Returns `true` if it advanced (the proof is fresh), `false` if it
    /// was stale or replayed. This is the durable, cross-restart replay guard —
    /// the only DB write the data plane conceptually triggers, exposed
    /// opaquely so the relay never touches the `tagmata` table.
    async fn bump_tunnel_proof_ts(
        &self,
        tagma_id: &TagmaId,
        ts: i64,
    ) -> Result<bool, ControlPlaneError>;
}
