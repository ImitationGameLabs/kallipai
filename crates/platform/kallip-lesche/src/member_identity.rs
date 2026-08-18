//! Resolve room members to their display identity (the stable `handle` + mutable
//! `label`) via the registry.
//!
//! This is the single home for "room-layer `MemberId` (a `ParticipantId` UUID) ->
//! display identity": it is shared by the roster (live membership) and the room
//! history read so the two paths can never diverge. A `MemberId` is the row's
//! stable identifier; the handle is a DERIVED display string (a function of that
//! id + mutable registry state -- the owner's username), so it is resolved here
//! at read time rather than persisted on a message row.

use std::collections::HashMap;

use kallip_agora_common::control_plane::{
    ControlPlane, ControlPlaneError, TagmaProfile, UserIdentity,
};
use kallip_agora_common::ids::{ParticipantKind, TagmaId, UserId};
use kallip_lesche_common::rooms::MemberId;

use crate::identity::{agent_handle, degraded_handle, human_handle};

/// A resolved participant identity: the stable display `handle`, the optional
/// mutable display `label` (an agent's owner-set label, or a human's
/// display_name), and -- for an agent -- its `tagma_id` (so the wire view can
/// deep-link a sender/roster row to that tagma's profile without reversing the
/// one-way participant id).
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub handle: String,
    pub label: Option<String>,
    pub tagma_id: Option<TagmaId>,
}

/// A member to resolve: its room-layer `id`, `kind`, and the underlying
/// `source_id` (`user_id` / `tagma_id`) the registry resolves by. Callers obtain
/// the `source_id` from wherever they hold it (the roster has the live
/// `room_members` row; the history read unions current and revoked membership)
/// -- this function does no DB access itself, so it stays pure and
/// unit-testable.
#[derive(Debug, Clone)]
pub struct MemberRef {
    pub id: MemberId,
    pub kind: ParticipantKind,
    pub source_id: String,
}

/// The degraded fallback [`ResolvedIdentity`] for a participant the registry did
/// not resolve (a since-deleted account, or a sender present in neither the live
/// nor revoked membership): the unforgeable `<kind> <short_prefix>` handle
/// (built by [`crate::identity::degraded_handle`]) with no label.
pub fn degraded(pid: &MemberId, kind: ParticipantKind) -> ResolvedIdentity {
    ResolvedIdentity {
        handle: degraded_handle(pid, kind),
        label: None,
        tagma_id: None,
    }
}

/// Resolve each participant's display identity via two batched registry RPCs
/// (agents -> `tagma_profiles`, humans -> `user_identities`). A participant the
/// registry resolves gets its real `label` + stable handle REGARDLESS of authz
/// state (revocation/disability are authz states, not display states); one the
/// registry does not resolve degrades to [`degraded`]. Returns a map keyed by
/// `ParticipantId`. Propagates a registry RPC error to the caller, which decides
/// whether to fail (roster) or degrade across the board (history read -- a
/// registry blip must never blank a history pull).
pub async fn resolve_handles(
    control: &dyn ControlPlane,
    refs: &[MemberRef],
) -> Result<HashMap<MemberId, ResolvedIdentity>, ControlPlaneError> {
    let mut out: HashMap<MemberId, ResolvedIdentity> = HashMap::with_capacity(refs.len());

    // Agents: one batched `tagma_profiles` read.
    let agent_refs: Vec<&MemberRef> = refs
        .iter()
        .filter(|r| r.kind == ParticipantKind::Agent)
        .collect();
    if !agent_refs.is_empty() {
        let ids: Vec<TagmaId> = agent_refs
            .iter()
            .map(|r| TagmaId::from(r.source_id.clone()))
            .collect();
        let resolved: HashMap<TagmaId, TagmaProfile> = control
            .tagma_profiles(&ids)
            .await?
            .into_iter()
            .map(|p| (p.tagma_id.clone(), p))
            .collect();
        for r in agent_refs {
            let tid = TagmaId::from(r.source_id.clone());
            let id = r.id.clone();
            out.insert(
                id.clone(),
                match resolved.get(&tid) {
                    // owner_username is present regardless of enrolled/revoked.
                    Some(p) => ResolvedIdentity {
                        handle: agent_handle(&r.id, &p.owner_username),
                        label: p.label.clone(),
                        tagma_id: Some(tid.clone()),
                    },
                    None => degraded(&r.id, r.kind),
                },
            );
        }
    }

    // Humans: one batched `user_identities` read.
    let human_refs: Vec<&MemberRef> = refs
        .iter()
        .filter(|r| r.kind == ParticipantKind::Human)
        .collect();
    if !human_refs.is_empty() {
        let ids: Vec<UserId> = human_refs
            .iter()
            .map(|r| UserId::from(r.source_id.clone()))
            .collect();
        let resolved: HashMap<UserId, UserIdentity> = control
            .user_identities(&ids)
            .await?
            .into_iter()
            .map(|u| (u.user_id.clone(), u))
            .collect();
        for r in human_refs {
            let uid = UserId::from(r.source_id.clone());
            out.insert(
                r.id.clone(),
                match resolved.get(&uid) {
                    Some(u) => ResolvedIdentity {
                        handle: human_handle(&u.username),
                        label: u.display_name.clone(),
                        tagma_id: None,
                    },
                    None => degraded(&r.id, r.kind),
                },
            );
        }
    }

    Ok(out)
}
