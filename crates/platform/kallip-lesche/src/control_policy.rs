//! Authorization predicates the relay derives LOCALLY over the registry's raw
//! tagma/user facts (see [`crate::control_plane_http`] / `ControlPlane`). The
//! registry is a fact service; it never renders a "usable for purpose X"
//! verdict -- that policy lives here, in the conversation/room domain.
//!
//! Each predicate mirrors a pre-consolidation agora oracle so behavior is
//! preserved exactly:
//! - [`tunnel_usable`] / the rooms-send gate = the old `tagma_identity` "Some"
//!   outcome (enrolled + non-revoked + pinned key).
//! - [`room_joinable`] = the old `tagma_enrolled` (enrolled + non-revoked +
//!   owner-not-disabled).
//! - [`bilateral_resolvable`] = the old `tagma_resolvable_by` (enrolled + owner
//!   match; deliberately NOT revoked-checked, matching prod).
//!
//! Call sites combine a fetch + a predicate into a single 404 with ONE fixed
//! literal regardless of which field failed, preserving the existence-oracle
//! (the client never learns *why* a tagma/user is unusable).

use kallip_agora_common::control_plane::{ControlPlane, ControlPlaneError, TagmaProfile};
use kallip_agora_common::ids::{TagmaId, UserId};

/// Fetch one tagma's profile (`None` if unknown). A thin wrapper over the
/// batched read for the single-id call sites (tunnel, rooms-send, the authz
/// gates) so they read as "resolve this one tagma".
pub(crate) async fn tagma_profile(
    cp: &dyn ControlPlane,
    tagma_id: &TagmaId,
) -> Result<Option<TagmaProfile>, ControlPlaneError> {
    Ok(cp
        .tagma_profiles(std::slice::from_ref(tagma_id))
        .await?
        .into_iter()
        .next())
}

/// The tunnel-reconnect / rooms-send usability gate: enrolled, non-revoked, and
/// carrying a pinned key (the reconnect proof is verified against it).
pub(crate) fn tunnel_usable(p: &TagmaProfile) -> bool {
    p.enrolled && !p.revoked && p.pinned_public_key.is_some()
}

/// The room-add gate: enrolled, non-revoked, owner not disabled.
pub(crate) fn room_joinable(p: &TagmaProfile) -> bool {
    p.enrolled && !p.revoked && !p.owner_disabled
}

/// The bilateral-conversation gate: enrolled and owned by `caller`. Deliberately
/// NOT revoked-checked, matching the historical `tagma_resolvable_by` (revocation
/// is enforced elsewhere -- the bearer gate + the tunnel proof).
pub(crate) fn bilateral_resolvable(p: &TagmaProfile, caller: &UserId) -> bool {
    p.enrolled && p.owner_user_id == *caller
}
