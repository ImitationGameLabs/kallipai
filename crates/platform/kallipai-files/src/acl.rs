//! The single ACL decision point for the files service.
//!
//! Every authorization decision goes through this module's function family:
//! the API layer parses the target into a [`SpacePath`], resolves a tagma
//! principal's [`EnrollmentFacts`] from the control plane (per request, no
//! cache), and asks the rules below. Path is authorization -- the two-layer
//! namespace (`/users/{user}/shared`, `/users/{user}/tagmas/{tagma}`) IS the
//! permission model, with no exception channel. Cross-principal transfer
//! exists only as server-side delivery (`api::send`), which writes as the
//! service and never passes through these rules.
//!
//! The matrix (nine rows as approved):
//!
//! | # | principal | target | right |
//! |---|---|---|---|
//! | 1 | User(U) | `/users/{U}/**` | full (space owner) |
//! | 2 | Tagma(T) | `/users/{U}/tagmas/{T}/**` | full, T enrolled in U |
//! | 3 | Tagma(T) | `/users/{U}/shared/**` | read/write, T enrolled in U |
//! | 4 | User(U1) | `/users/{U2}/**`, U1 != U2 | denied (cross-user hard isolation) |
//! | 5 | Tagma(T1) | `/users/{U}/tagmas/{T2}/**`, T1 != T2 | denied |
//! | 6 | Tagma(T1) -> T2 | single file | server-side delivery only |
//! | 7 | User(U1) -> U2 | room reference | not adopted (postponed) |
//! | 8 | Admin | content | management face only; blob content denied |
//! | 9 | User(U1) | `/users/{U2}/inbox/` | no path right; delivery only |
//!
//! Rows 6/7/9 are non-grants: they live in the send flow (6, 9) or nowhere
//! (7), and the rules below deliberately deny the corresponding paths so the
//! delivery write stays the only road in.

use std::collections::BTreeSet;

/// The parsed form of a space path (`/users/{user}/...`). Anything outside a
/// user space does not parse: the files service has no other namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpacePath {
    /// The user space this path lives in.
    pub user: String,
    /// The area within the user space the path points into.
    pub area: Area,
}

/// One area of a user space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Area {
    /// `/shared` -- the shared read/write region.
    Shared,
    /// `/tagmas/{tagma}` -- one tagma's private region (`inbox/` inside it
    /// included).
    Tagma { tagma: String },
    /// `/inbox` -- the user's delivery landing directory, owned by the user
    /// like the rest of their space (row 1) and invisible to tagma path
    /// rights (rows 2/3 never reach it).
    Inbox,
}

impl SpacePath {
    /// Parse a space path. Returns `None` for anything that is not a
    /// well-formed path inside a user space (a missing/empty user or area
    /// segment, or an unknown top-level area).
    pub fn parse(path: &str) -> Option<Self> {
        let rest = path.strip_prefix("/users/")?;
        let (user, rest) = rest.split_once('/')?;
        if user.is_empty() {
            return None;
        }
        let (first, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let area = match first {
            "shared" => Area::Shared,
            "inbox" => Area::Inbox,
            "tagmas" => {
                let (tagma, _) = tail.split_once('/').unwrap_or((tail, ""));
                if tagma.is_empty() {
                    return None;
                }
                Area::Tagma {
                    tagma: tagma.to_owned(),
                }
            }
            _ => return None,
        };
        Some(Self {
            user: user.to_owned(),
            area,
        })
    }
}

/// What the caller wants to do. Send is deliberately absent: its source-side
/// read is an [`Action::Read`] and its landing write is the service's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read content (GET/HEAD) or read metadata for a decision.
    Read,
    /// Put new content under a path.
    Write,
    /// Remove a record.
    Delete,
}

/// A tagma principal's enrollment facts, resolved per request from the
/// control plane. The consumer contract: a missing set
/// is denial -- no code path may structurally assume the requester's own
/// presence in `enrolled`.
#[derive(Debug, Clone)]
pub struct EnrollmentFacts {
    /// The user space the tagma belongs to.
    pub space_user: String,
    /// The space's enrolled, non-revoked tagmas (point-in-time snapshot).
    pub enrolled: BTreeSet<String>,
}

/// Row #8's content face: the admin principal never touches blob content.
/// Its management surface (the delivery-events query) is admin-guarded in
/// `api::admin`, not an ACL grant.
pub fn admin_can_content() -> bool {
    false
}

/// Rows #1/#4/#9: a user principal against a parsed space path. Full rights
/// in the user's own space -- shared region, the tagmas' private regions
/// (arranging, not sending), and the inbox -- and every other user's space
/// denied -- the hard cross-user isolation. Row 9 (delivery into another user's
/// inbox) is unreachable here by design: it is a server-side write in the
/// send flow, never a path grant.
pub fn user_can(user: &str, path: &SpacePath, _action: Action) -> bool {
    path.user == user
}

/// Rows #2/#3/#5: a tagma principal against a parsed space path, given its
/// enrollment facts. Full rights inside its own private region (row 2),
/// read/write in the shared region (row 3), denial everywhere else (row 5):
/// another tagma's private region, the user-space inbox, any space it is not
/// enrolled in. Missing facts deny everything -- absence is denial.
pub fn tagma_can(tagma: &str, path: &SpacePath, _action: Action, facts: &EnrollmentFacts) -> bool {
    if facts.space_user != path.user || !facts.enrolled.contains(tagma) {
        return false;
    }
    match &path.area {
        Area::Tagma { tagma: area_tagma } => area_tagma == tagma,
        Area::Shared => true,
        Area::Inbox => false,
    }
}

/// The delete decision for a concrete record (its parsed path plus its
/// `owner` field): a user deletes anything in their own space (row 1); a
/// tagma deletes freely inside its private region (row 2) and only its own
/// writes in the shared region (row 3's owner-field refinement).
pub fn can_delete_record(
    who: PrincipalRef<'_>,
    path: &SpacePath,
    record_owner: &str,
    facts: &EnrollmentFacts,
) -> bool {
    match who {
        PrincipalRef::User(user) => user_can(user, path, Action::Delete),
        PrincipalRef::Tagma(tagma) => {
            tagma_can(tagma, path, Action::Delete, facts)
                && match path.area {
                    Area::Shared => record_owner == tagma,
                    _ => true,
                }
        }
    }
}

/// Which kind of principal a delete decision is for (the API layer's
/// resolved identity, borrowed as string slices).
#[derive(Debug, Clone, Copy)]
pub enum PrincipalRef<'a> {
    /// A cookie-authenticated user.
    User(&'a str),
    /// A bearer-authenticated tagma.
    Tagma(&'a str),
}

/// The send-flow target guard behind rows 6/9: `member` may receive a
/// delivery only if it is enrolled in the SAME user space the facts were
/// resolved for. The caller resolves facts once per request (its own lookup
/// as a tagma, or the target tagma's lookup when the caller is a user) and
/// passes the space user it intends to deliver within. Missing facts deny.
pub fn can_receive_delivery(facts: &EnrollmentFacts, space_user: &str, member: &str) -> bool {
    facts.space_user == space_user && facts.enrolled.contains(member)
}

#[cfg(test)]
mod tests {
    //! The nine matrix rows, pinned. Rows 6/7/9 are pinned as non-grants:
    //! the corresponding paths deny here so the delivery write stays the
    //! only road in (rows 6/9 live in `api::send`).

    use super::*;

    fn facts(user: &str, members: &[&str]) -> EnrollmentFacts {
        EnrollmentFacts {
            space_user: user.to_owned(),
            enrolled: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn path(s: &str) -> SpacePath {
        SpacePath::parse(s).expect("test path parses")
    }

    // --- parse ---

    #[test]
    fn parse_distinguishes_areas() {
        assert_eq!(
            path("/users/u1/shared/a.txt"),
            SpacePath {
                user: "u1".into(),
                area: Area::Shared
            }
        );
        assert_eq!(
            path("/users/u1/tagmas/t1/inbox/a.txt"),
            SpacePath {
                user: "u1".into(),
                area: Area::Tagma { tagma: "t1".into() }
            }
        );
        assert_eq!(
            path("/users/u1/inbox/a.txt"),
            SpacePath {
                user: "u1".into(),
                area: Area::Inbox
            }
        );
    }

    #[test]
    fn parse_rejects_malformed_paths() {
        assert!(SpacePath::parse("users/u1/shared/a.txt").is_none());
        assert!(SpacePath::parse("/users//shared/a.txt").is_none());
        assert!(SpacePath::parse("/users/u1").is_none());
        assert!(SpacePath::parse("/users/u1/mystery/a.txt").is_none());
        assert!(SpacePath::parse("/users/u1/tagmas//a.txt").is_none());
        assert!(SpacePath::parse("/shared/a.txt").is_none());
    }

    // --- row 1: user owns the whole space ---

    #[test]
    fn row1_user_full_rights_in_own_space() {
        let u = "u1";
        for p in [
            "/users/u1/shared/a.txt",
            "/users/u1/tagmas/t1/anything.bin",
            "/users/u1/inbox/a.txt",
        ] {
            for a in [Action::Read, Action::Write, Action::Delete] {
                assert!(user_can(u, &path(p), a), "{a:?} {p}");
            }
        }
    }

    // --- row 4: cross-user hard isolation ---

    #[test]
    fn row4_user_denied_in_other_user_space_everywhere() {
        for p in [
            "/users/u2/shared/a.txt",
            "/users/u2/tagmas/t2/a.bin",
            "/users/u2/inbox/a.txt",
        ] {
            for a in [Action::Read, Action::Write, Action::Delete] {
                assert!(!user_can("u1", &path(p), a), "{a:?} {p}");
            }
        }
    }

    // --- row 9: no path grant into another user's inbox ---

    #[test]
    fn row9_delivery_target_is_not_a_path_grant() {
        // The U2U landing path denies as a path right; only api::send's
        // service write lands there (validated against row-6-style delivery
        // guards, never against this rule).
        assert!(!user_can(
            "u1",
            &path("/users/u2/inbox/a.txt"),
            Action::Write
        ));
    }

    // --- row 2: tagma owns its private region ---

    #[test]
    fn row2_tagma_full_rights_in_own_private_region() {
        let f = facts("u1", &["t1", "t2"]);
        for p in [
            "/users/u1/tagmas/t1/a.bin",
            "/users/u1/tagmas/t1/inbox/a.txt",
        ] {
            for a in [Action::Read, Action::Write, Action::Delete] {
                assert!(tagma_can("t1", &path(p), a, &f), "{a:?} {p}");
            }
        }
    }

    // --- row 3: tagma read/write in shared ---

    #[test]
    fn row3_tagma_read_write_in_shared_region() {
        let f = facts("u1", &["t1"]);
        for a in [Action::Read, Action::Write] {
            assert!(tagma_can("t1", &path("/users/u1/shared/a.txt"), a, &f));
        }
    }

    #[test]
    fn row3_shared_delete_follows_the_owner_field() {
        let f = facts("u1", &["t1", "t2"]);
        let p = path("/users/u1/shared/a.txt");
        // Own write: deletable. Another tagma's write: not.
        assert!(can_delete_record(PrincipalRef::Tagma("t1"), &p, "t1", &f));
        assert!(!can_delete_record(PrincipalRef::Tagma("t1"), &p, "t2", &f));
        // The space owner deletes anything.
        assert!(can_delete_record(PrincipalRef::User("u1"), &p, "t2", &f));
    }

    // --- row 5: tagma denied in other private regions and foreign spaces ---

    #[test]
    fn row5_tagma_denied_outside_its_region_and_space() {
        let f = facts("u1", &["t1"]);
        // Another tagma's private region in the same space.
        assert!(!tagma_can(
            "t1",
            &path("/users/u1/tagmas/t2/a.bin"),
            Action::Read,
            &f
        ));
        // The user-space inbox is not a tagma surface.
        assert!(!tagma_can(
            "t1",
            &path("/users/u1/inbox/a.txt"),
            Action::Write,
            &f
        ));
        // A foreign space entirely.
        let foreign = facts("u9", &["t9"]);
        assert!(!tagma_can(
            "t1",
            &path("/users/u9/shared/a.txt"),
            Action::Read,
            &foreign
        ));
    }

    // --- facts discipline: absence is denial ---

    #[test]
    fn missing_facts_deny_every_tagma_decision() {
        let empty = facts("u1", &[]);
        assert!(!tagma_can(
            "t1",
            &path("/users/u1/tagmas/t1/a.bin"),
            Action::Read,
            &empty
        ));
        // The facts naming another space deny even a shape-valid membership.
        let foreign = facts("u2", &["t1"]);
        assert!(!tagma_can(
            "t1",
            &path("/users/u1/shared/a.txt"),
            Action::Read,
            &foreign
        ));
    }

    // --- row 8: admin content face closed ---

    #[test]
    fn row8_admin_content_is_denied() {
        // The rule is a constant denial: the management surface lives in
        // api::admin behind its own guard, and every content operation
        // refuses the admin principal.
        assert!(!admin_can_content());
    }

    // --- rows 6: delivery guards ---

    #[test]
    fn row6_delivery_needs_same_space_membership() {
        let f = facts("u1", &["t1", "t2"]);
        assert!(can_receive_delivery(&f, "u1", "t2"));
        // Cross-space target: denied even if the id is somehow a member
        // elsewhere.
        let other = facts("u9", &["t9"]);
        assert!(!can_receive_delivery(&other, "u1", "t9"));
        // Absent member: denied.
        assert!(!can_receive_delivery(&f, "u1", "t9"));
    }

    // --- row 2 vs row 6: the delivery road is the service's, not a grant ---

    #[test]
    fn target_region_denies_other_tagmas_even_for_delivery_landing() {
        // T1 has no path right into T2's inbox; the send flow writes there
        // as the service after can_receive_delivery passes. Pinned so nobody
        // "simplifies" the ACL into granting it.
        let f = facts("u1", &["t1", "t2"]);
        assert!(!tagma_can(
            "t1",
            &path("/users/u1/tagmas/t2/inbox/a.txt"),
            Action::Write,
            &f
        ));
    }
}
