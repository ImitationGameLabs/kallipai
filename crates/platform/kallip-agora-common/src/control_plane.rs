//! The narrow interface the data-plane relay (`kallip-lesche`) uses to talk to
//! the registry (`kallip-agora`). The registry owns identity and credentials
//! only; the relay owns all conversation/room state and policy. This trait is
//! therefore a **fact service**: the relay asks the registry for raw identity
//! facts (who owns a tagma, their label/handle/key/enrollment state) and
//! derives every authorization decision itself -- the registry never renders a
//! "usable for purpose X" verdict. Authentication (cookie/token verification)
//! and the tunnel-proof replay guard are the only non-read surface.
//!
//! Keeping the surface small and stable is the point of the control-plane /
//! data-plane split: app↔tagma business evolution happens inside the lesche
//! and the shared wire types, never here. The lesche runs as a separate service
//! and reaches this trait over the `/internal/*` HTTP API via an RPC client impl
//! (`HttpControlPlane`); the on-wire contract for that API lives in
//! [`crate::internal_api`].

use crate::bytes::Ed25519PublicKey;
use crate::ids::{TagmaId, UserId};
use crate::principal::Principal;
use serde::{Deserialize, Serialize};

/// A tagma's registry facts: the raw identity + usability state the relay reads
/// to (a) verify a tunnel-reconnect proof against `pinned_public_key`, (b) stamp
/// the authoritative display identity (`label` + `@owner_username`) onto room
/// envelopes/rows, and (c) derive locally whether the tagma may join a room /
/// open a bilateral chat (`enrolled && !revoked && !owner_disabled`, etc.). The
/// registry returns these facts UNFILTERED -- one row per existing input id --
/// so the relay, not the registry, owns the policy that combines them. A tagma
/// missing from the result is simply unknown (omitted); the relay degrades its
/// roster row to a prefix-only handle.
/// The serde form IS the `tagma-profiles` wire contract -- `TagmaProfileResponse`
/// in [`crate::internal_api`] is a type alias of this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaProfile {
    pub tagma_id: TagmaId,
    /// The Ed25519 public key pinned at enrollment (`None` while pending).
    pub pinned_public_key: Option<Ed25519PublicKey>,
    /// The user who owns this tagma (receives its presence + envelopes).
    pub owner_user_id: UserId,
    /// The owner-set display name (mutable; the tagma cannot self-declare it).
    pub label: Option<String>,
    /// The owner's login handle (NOT NULL, unique); rendered as `@owner` so a
    /// participant can see who endorses / is accountable for the agent.
    pub owner_username: String,
    /// The owner's optional display name.
    pub owner_display_name: Option<String>,
    /// `enrolled_at.is_some()` -- pending vs enrolled.
    pub enrolled: bool,
    /// `revoked_at.is_some()` -- the unified revoke flag.
    pub revoked: bool,
    /// `owner.disabled_at.is_some()` -- a disabled owner's tagmas cannot join
    /// rooms (matches the historical `tagma_enrolled` gate, now derived here).
    pub owner_disabled: bool,
}

/// A user's registry facts: the raw identity + state the relay reads to label a
/// HUMAN roster member and to derive locally whether they may be invited
/// (`!disabled`). The registry returns rows UNFILTERED; a user missing from the
/// result is unknown (omitted).
/// The serde form IS the user-identity wire contract (both the bulk and the
/// by-username reads) -- `UserIdentityResponse` in [`crate::internal_api`]
/// is a type alias of this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIdentity {
    pub user_id: UserId,
    pub username: String,
    pub display_name: Option<String>,
    /// `disabled_at.is_some()` -- a disabled user cannot be invited (matches the
    /// historical `user_exists` gate, now derived here).
    pub disabled: bool,
}

/// A verified user session: the authenticated user's id plus the display
/// identity resolved at connection-open. `username` is consumed at connect to
/// stamp the stable `@username` handle onto live human-sent room envelopes;
/// `display_name` is resolved for the roster/history label. The durable room
/// message row persists only the stable `ParticipantId`, never a handle. Resolved
/// once per connection-open alongside the auth check, not per-message -- the
/// relay caches it for the connection lifetime, matching the no-auth-cache /
/// low-RPC-volume design of the control plane.
/// The serde form IS the verify-session wire contract --
/// `VerifySessionResponse` in [`crate::internal_api`] is a type alias of
/// this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedSession {
    pub user_id: UserId,
    /// Login name; always present (a fallback render when `display_name` is
    /// unset).
    pub username: String,
    /// Optional human label (the user's chosen display name).
    pub display_name: Option<String>,
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
    /// Verify a `kallip_session` cookie value -> the owning user plus their
    /// authoritative display identity, or `None` if the session is absent /
    /// expired / disabled. By construction this can only ever produce a `User`
    /// (the deputy guard: a `User` is reachable ONLY via the cookie). The relay
    /// resolves this once per connection-open and caches the display for the
    /// connection lifetime (it is NOT a per-message call).
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError>;

    /// Verify an `Authorization: Bearer` token -> an `Admin` or `Tagma`
    /// principal, or `None` if invalid / revoked / owner-disabled. (`Admin` is
    /// returned but rejected by the relay's `require_tagma` on data-plane
    /// routes, matching the registry's own behavior.)
    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError>;

    /// Batched tagma-facts resolve: one [`TagmaProfile`] per existing input id,
    /// UNFILTERED (raw enrolled/revoked/owner-disabled state + key + display
    /// fields). Unknown ids are omitted, never error'd. The single read behind
    /// the tunnel-reconnect proof, the rooms-send handle stamp, and the room
    /// roster's agent labels; the relay derives all authorization from the
    /// returned facts.
    async fn tagma_profiles(
        &self,
        tagma_ids: &[TagmaId],
    ) -> Result<Vec<TagmaProfile>, ControlPlaneError>;

    /// Batched user-facts resolve: one [`UserIdentity`] per existing input id,
    /// UNFILTERED (raw `disabled` state + display fields). Unknown ids are
    /// omitted. Callers always resolve a known set of ids in bulk (the room
    /// roster's human labels; the invite inbox's inviter handles), deriving
    /// `!disabled` locally.
    async fn user_identities(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<UserIdentity>, ControlPlaneError>;

    /// Resolve a single human principal by login handle. The registry
    /// normalizes a bare handle via the same rules as signup; the caller strips
    /// any `@` sigil first. An unknown or malformed handle collapses to `None`
    /// (no shape leak -- the relay renders one fixed 404). A disabled user is
    /// returned as `Some(disabled = true)`, UNFILTERED, matching
    /// [`user_identities`](Self::user_identities); the invite gate derives
    /// `!disabled` locally so the disabled and unknown cases stay
    /// indistinguishable to the caller. The invite gate is the sole caller: it
    /// addresses an invitee by the handle a member actually knows, then reads
    /// the resolved `user_id` back out of the returned identity.
    async fn user_identity_by_username(
        &self,
        username: &str,
    ) -> Result<Option<UserIdentity>, ControlPlaneError>;

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
