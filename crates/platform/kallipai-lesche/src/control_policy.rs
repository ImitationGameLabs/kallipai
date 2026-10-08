//! Authorization predicates the relay derives LOCALLY over the registry's raw
//! tagma/user facts (see [`crate::control_plane_http`] / `ControlPlane`). The
//! registry is a fact service; it never renders a "usable for purpose X"
//! verdict -- that policy lives here, in the conversation/room domain.
//!
//! Each predicate mirrors an archeion oracle gate so behavior is
//! preserved exactly:
//! - [`agent_deliverable`] / the send gates = the old `tagma_identity` "Some"
//!   outcome (enrolled + non-revoked + pinned key).
//! - [`room_joinable`] = the old `tagma_enrolled` (enrolled + non-revoked +
//!   owner-not-disabled).
//!
//! Call sites combine a fetch + a predicate into a single 404 with ONE fixed
//! literal regardless of which field failed, preserving the existence-oracle
//! (the client never learns *why* a tagma/user is unusable).

use kallipai_archeion_common::control_plane::{ControlPlane, ControlPlaneError, TagmaProfile};
use kallipai_archeion_common::ids::TagmaId;

/// Fetch one tagma's profile (`None` if unknown). A thin wrapper over the
/// batched read for the single-id call sites (rooms-send, direct, the authz
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

/// The send-path deliverability gate: enrolled, non-revoked, and carrying a
/// pinned key (the keryx reconnect proof verifies against it).
pub(crate) fn agent_deliverable(p: &TagmaProfile) -> bool {
    p.enrolled && !p.revoked && p.pinned_public_key.is_some()
}

/// The room-add gate: enrolled, non-revoked, owner not disabled.
pub(crate) fn room_joinable(p: &TagmaProfile) -> bool {
    p.enrolled && !p.revoked && !p.owner_disabled
}
