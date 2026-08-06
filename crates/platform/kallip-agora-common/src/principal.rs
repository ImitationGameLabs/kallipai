//! The resolved identity of an agora request, and the authorization helpers
//! that gate handlers on it.
//!
//! `Principal` and the `require_*` helpers live in this shared crate so both
//! the registry (`kallip-agora`) and the data-plane relay (`kallip-lesche`)
//! enforce the same deputy guard. The credential-to-Principal resolution
//! (cookie / bearer verification against the durable store) is a registry
//! concern and lives behind the [`crate::control_plane::ControlPlane`] trait;
//! the lesche consumes the resulting `Principal` (over the `/internal/*` HTTP
//! API, where it is carried as [`crate::internal_api::WirePrincipal`]).
//!
//! # Origin / deputy guard
//!
//! A `User` principal is reached ONLY via the `kallip_session` cookie; a
//! `Tagma` principal is reached ONLY via an `sk-tagma-` bearer; the admin ONLY
//! via an `sk-admin-` bearer. So the deputy threats a multi-origin design would
//! face — a session cookie authenticating a tagma route, a tagma bearer
//! reaching `/v1/me` — are already impossible by construction: `require_tagma`
//! never sees a `User` and `require_user` never sees a `Tagma`.

use crate::ids::{ParticipantId, ParticipantKind, TagmaId, UserId};
use kallip_common::protocol::ApiError;

/// Resolved identity from either an `Authorization: Bearer` header (admin /
/// tagma) or the `kallip_session` cookie (user).
#[derive(Debug, Clone)]
pub enum Principal {
    Admin,
    /// A signed-in user. Reached via the session cookie.
    User(UserId),
    /// A tagma's long-lived tagma token. Always via bearer.
    Tagma(TagmaId),
}

impl Principal {
    /// The room-participant id this principal maps to, or `None` for `Admin`
    /// (which is not a room participant). Room-surface routes use this so the
    /// room layer never sees the underlying `UserId`/`TagmaId`.
    pub fn participant_id(&self) -> Option<ParticipantId> {
        match self {
            Principal::Admin => None,
            Principal::User(id) => Some(ParticipantId::for_user(id)),
            Principal::Tagma(id) => Some(ParticipantId::for_tagma(id)),
        }
    }

    /// The room-participant kind, or `None` for `Admin`.
    pub fn participant_kind(&self) -> Option<ParticipantKind> {
        match self {
            Principal::Admin => None,
            Principal::User(_) => Some(ParticipantKind::Human),
            Principal::Tagma(_) => Some(ParticipantKind::Agent),
        }
    }
}

/// Only the admin may proceed (provisioning endpoints).
pub fn require_admin(principal: &Principal) -> Result<(), ApiError> {
    match principal {
        Principal::Admin => Ok(()),
        _ => Err(ApiError::forbidden("admin access required")),
    }
}

/// The authenticated user (for user-scoped endpoints). Reached via the session
/// cookie.
pub fn require_user(principal: &Principal) -> Result<&UserId, ApiError> {
    match principal {
        Principal::User(id) => Ok(id),
        _ => Err(ApiError::forbidden("user access required")),
    }
}

/// The authenticated tagma (for the tagma routes). Reached via tagma bearer
/// only; a cookie-sourced principal never resolves to `Tagma`, so this is the
/// deputy guard for tagma routes.
pub fn require_tagma(principal: &Principal) -> Result<&TagmaId, ApiError> {
    match principal {
        Principal::Tagma(id) => Ok(id),
        _ => Err(ApiError::forbidden("tagma access required")),
    }
}

/// The authenticated room participant (for the kind-agnostic room routes). A
/// `User` resolves to its derived `ParticipantId` (Human); a `Tagma` to its
/// derived `ParticipantId` (Agent); `Admin` is rejected (it is not a room
/// participant). The underlying `UserId`/`TagmaId` is deliberately not returned
/// -- the room surface deals only in `ParticipantId`. Routes that need a
/// specific kind use [`require_user`] / [`require_tagma`] instead.
pub fn require_participant(
    principal: &Principal,
) -> Result<(ParticipantId, ParticipantKind), ApiError> {
    match (principal.participant_id(), principal.participant_kind()) {
        // Both helpers agree (User -> Human, Tagma -> Agent); Admin maps to
        // (None, None). Routed through the two helpers so the principal-to-
        // participant mapping lives in exactly one place.
        (Some(id), Some(kind)) => Ok((id, kind)),
        _ => Err(ApiError::forbidden("a room participant is required")),
    }
}
