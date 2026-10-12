//! The authorization matrix: which peer uid may act on which target
//! uid, and the delegate refusals that narrow it.

use super::error::SpawnError;

/// Env keys the daemon owns; a request may not override them.
/// (`KALLIPAI_TAGMA_ADDR` is deliberately absent — it is the one user-set
/// listen knob: the daemon defaults it, a request pair wins, and the
/// value is shape-checked at validation.)
pub(super) const RESERVED_KEYS: [&str; 3] = [
    "KALLIPAI_TAGMA_SLUG",
    "KALLIPAI_WORKSPACE_ROOT",
    "KALLIPAI_TAGMA_DATA_DIR",
];

/// The launch authorization (platform-hosting access rule): an
/// instance may run as the requesting peer itself, as any declared
/// user when the peer is root (the admin exemption behind
/// `sudo kallipctl`), or — for peers on the delegates list — as any
/// declared user (the delegated-administration grant fed by the
/// deployment). Called before any state change — in particular
/// before the slug collision probe — so a denied peer cannot probe
/// slug existence through error deltas.
pub(super) fn authorized_with(
    delegates: &std::collections::HashSet<u32>,
    peer_uid: u32,
    target_uid: u32,
) -> bool {
    peer_uid == target_uid || peer_uid == 0 || delegates.contains(&peer_uid)
}

/// The process-wide view: the grant parsed at startup (empty unless
/// the deployment configured one).
pub(crate) fn authorized(peer_uid: u32, target_uid: u32) -> bool {
    authorized_with(crate::delegates::current(), peer_uid, target_uid)
}

/// The keyless form is always a refusal for a delegate peer: it would
/// silently claim the instance for the delegate's own service
/// account, which owns nothing. Pure so each verb (and each test)
/// drives the same check.
pub(crate) fn delegate_keyless_rejection(
    delegates: &std::collections::HashSet<u32>,
    peer_uid: u32,
) -> Option<SpawnError> {
    if delegates.contains(&peer_uid) {
        Some(SpawnError::Invalid(format!(
            "peer uid {peer_uid} is a delegate: name the launch user explicitly"
        )))
    } else {
        None
    }
}

/// The targets a delegate peer may not name: root (tagma's own boot
/// guard would be the only backstop), the daemon's own account (an
/// instance inside the daemon's identity holds the record area), and
/// another delegate account (service accounts own nothing). Pure for
/// the same reason; the refusal carries the alternative action.
pub(super) fn delegate_target_rejection(
    delegates: &std::collections::HashSet<u32>,
    peer_uid: u32,
    target_uid: u32,
    daemon_uid: u32,
) -> Option<SpawnError> {
    if !delegates.contains(&peer_uid) {
        return None;
    }
    if target_uid == 0 {
        return Some(SpawnError::Invalid(
            "delegate peers may not launch instances as root; name a declared user instead"
                .to_string(),
        ));
    }
    if target_uid == daemon_uid {
        return Some(SpawnError::Invalid(
            "delegate peers may not launch instances as the daemon's own account; name a declared user instead"
                .to_string(),
        ));
    }
    if delegates.contains(&target_uid) {
        return Some(SpawnError::Invalid(
            "delegate peers may not target another delegate account; name a declared user instead"
                .to_string(),
        ));
    }
    None
}
