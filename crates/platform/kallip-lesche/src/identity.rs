//! Pure room-identity construction: the stable handle, its short anchor, and
//! the degraded fallback. These are the single home for the handle vocabulary
//! used by both the message-stamp path (`crate::routes::rooms`) and the
//! registry-backed resolver (`crate::member_identity`, which the roster
//! and the history read share). The registry resolution itself lives in
//! `member_identity`; this module is the pure layer it builds on.
//!
//! The stable handle is the unforgeable anchor rendered alongside (never as
//! part of) a mutable display name: `<id-prefix>@<owner-username>` for an agent,
//! `@<username>` for a human. The id-prefix is a few chars of the server-derived
//! id -- a `ParticipantId` on the send path, a `MemberId` at the resolver; the
//! helpers here are generic over `AsRef<str>` so both share one vocabulary. The
//! `@owner`/`@username` is the endorsement. A mimic can forge neither, so two
//! same-named agents stay distinguishable and a spoofed client-supplied handle
//! never survives relay stamping.

use kallip_agora_common::ids::ParticipantKind;

/// The first chars of an id: the unforgeable short anchor shown alongside a
/// display name so two same-named members stay distinct. The id is
/// server-derived, so a member cannot choose or forge it. Capped at 6 chars
/// (room tagmas' v5 ids are far longer); a shorter id yields its full length.
/// Char-based (not byte-sliced) so a future non-ASCII id source cannot panic on
/// a code-point boundary. Generic over `AsRef<str>` so it serves both the wire
/// `ParticipantId` (send path) and the room-domain `MemberId` (resolver).
pub(crate) fn short_prefix<T: AsRef<str>>(pid: &T) -> String {
    pid.as_ref().chars().take(6).collect()
}

/// The authoritative STABLE handle for an agent sender: `<id-prefix>@<owner>`.
/// The prefix is the unforgeable server-derived id; the owner username is who
/// endorses / is accountable for the agent. This is a handle (stable, unique),
/// NOT a display name: the mutable label is resolved separately and prepended
/// only at render. Generic over `AsRef<str>` (see [`short_prefix`]).
pub(crate) fn agent_handle<T: AsRef<str>>(pid: &T, owner_username: &str) -> String {
    format!("{}@{}", short_prefix(pid), owner_username)
}

/// The authoritative STABLE handle for a human sender: `@<username>`. The
/// `@username` is the endorsement -- unforgeable because the relay stamps it
/// from the registry-resolved login handle, never from client input. The human
/// counterpart to [`agent_handle`]; one home so the message stamp, the roster
/// resolver, and the invite inbox agree on the same text.
pub(crate) fn human_handle(username: &str) -> String {
    format!("@{}", username)
}

/// The degraded display handle for a member the registry did not resolve: the
/// unforgeable `<kind> <short_prefix>` form, with no owner/username endorsement.
/// Used by the send-path stamp (a registry miss at send time) and by
/// `member_identity::resolve_handles` (an unresolved member at read time) so the
/// two paths share ONE fallback vocabulary. Generic over `AsRef<str>` (see
/// [`short_prefix`]).
pub(crate) fn degraded_handle<T: AsRef<str>>(pid: &T, kind: ParticipantKind) -> String {
    format!("{} {}", display_kind_word(kind), short_prefix(pid))
}

/// The kind word used in the degraded handle. This diverges from
/// [`ParticipantKind::as_str`] (which is `"human"`) to `"user"` for byte-identical
/// parity with the historical send-path fallback -- the single source of truth
/// for that choice lives here.
pub(crate) fn display_kind_word(kind: ParticipantKind) -> &'static str {
    match kind {
        ParticipantKind::Agent => "agent",
        ParticipantKind::Human => "user",
    }
}
