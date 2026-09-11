//! Startup guard against running with the host's real uid 0.
//!
//! Everything this tagma spawns inherits its privileges, and the sandbox
//! stack (Landlock, seccomp) does not reclaim uid capabilities — a real-root
//! tagma turns one hallucinated command into a host-wide incident. "Real
//! root" is a property of the user namespace, not of the euid alone: fleet
//! sandboxes run with euid 0 inside a nested namespace whose map translates
//! uid 0 to an unprivileged host user, which is safe. The probe reads
//! `/proc/self/uid_map`; the initial namespace is exactly the one whose map
//! is the full identity range `0 0 4294967295` (user_namespaces(7)). An
//! unreadable or unparseable map fails closed: an euid-0 process that cannot
//! prove its namespace is nested is refused.

use std::process::exit;

/// Escape hatch for rootful containers without user-namespace remapping.
/// The literal `"1"` and nothing else unlocks; the value is consulted only
/// when the process would otherwise be refused.
pub const ACCEPT_UNSAFE_RUN_AS_ROOT: &str = "KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT";

/// The startup verdict for the current process.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Either not root, or root inside a mapped (non-initial) namespace.
    Allow,
    /// Real root, explicitly unlocked via [`ACCEPT_UNSAFE_RUN_AS_ROOT`].
    AllowUnsafeRoot,
    /// Real root without consent; the string is the actionable refusal.
    Refuse(String),
}

/// Pure core of the guard, fully matrix-tested; [`enforce`] supplies the IO.
///
/// The escape value changes nothing for a non-root or mapped-root boot —
/// it is only read at the point a refusal would otherwise happen.
pub fn decide(euid_is_root: bool, initial_userns: bool, escape: Option<&str>) -> Decision {
    if !euid_is_root || !initial_userns {
        return Decision::Allow;
    }
    match escape {
        Some("1") => Decision::AllowUnsafeRoot,
        _ => Decision::Refuse(format!(
            "refusing to run as the host's real root: uid 0 here maps to the machine's \
             uid 0, and every instance this tagma spawns would inherit full host \
             privileges (the sandbox stack does not reclaim uid capabilities). Run \
             tagma as a dedicated unprivileged user instead (a system user started \
             by systemd under `User=`, never root). Only where that is impossible — \
             a rootful container without user-namespace remapping — set \
             {ACCEPT_UNSAFE_RUN_AS_ROOT}=1 to accept the risk."
        )),
    }
}

/// True iff the map is the initial namespace's full identity range: a single
/// line whose three fields are exactly `0 0 4294967295`. Remapped ranges,
/// extra lines, and garbage all read as non-initial; the fail-closed side of
/// unparseable content is `probe_initial`'s job, not the parser's.
pub fn uid_map_is_initial(content: &str) -> bool {
    let mut fields = content.split_whitespace();
    matches!(
        (fields.next(), fields.next(), fields.next(), fields.next()),
        (Some("0"), Some("0"), Some("4294967295"), None)
    )
}

/// True iff every line is three decimal fields — the shape of anything the
/// kernel actually wrote. Distinguishes "a nested namespace" (allowable for
/// root) from "content we cannot interpret" (handled as probe failure).
fn well_formed_uid_map(content: &str) -> bool {
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.len() == 3 && fields.iter().all(|f| f.parse::<u32>().is_ok())
        })
}

fn read_uid_map() -> std::io::Result<String> {
    std::fs::read_to_string("/proc/self/uid_map")
}

/// euid-0 callers only: is this the initial namespace? Anything short of
/// positive evidence of a nested namespace — unreadable map, unparseable
/// content — answers yes, so the refusal below fires rather than a guess.
fn probe_initial() -> bool {
    let content = match read_uid_map() {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(%error, "uid_map unreadable; failing closed");
            return true;
        }
    };
    if !well_formed_uid_map(&content) {
        tracing::warn!("uid_map content unparseable; failing closed");
        return true;
    }
    uid_map_is_initial(&content)
}

/// Wire-up called at the very top of `run`: logging is live by then and no
/// filesystem state, socket, or agent surface exists yet, so a refusal lands
/// before `boot_identity` touches the slug tree.
pub fn enforce() {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let escape = std::env::var(ACCEPT_UNSAFE_RUN_AS_ROOT).ok();
    match decide(true, probe_initial(), escape.as_deref()) {
        Decision::Allow => {}
        Decision::AllowUnsafeRoot => tracing::warn!(
            "running as real root via {ACCEPT_UNSAFE_RUN_AS_ROOT}=1 — unsafe; every \
             spawned instance wields host-root privileges"
        ),
        Decision::Refuse(reason) => {
            tracing::error!(%reason, "startup refused");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- decide: the full input matrix ------------------------------------

    #[test]
    fn a_non_root_boot_allows_regardless_of_namespace_or_escape() {
        for initial in [true, false] {
            for escape in [None, Some(""), Some("0"), Some("1"), Some("true")] {
                assert_eq!(
                    decide(false, initial, escape),
                    Decision::Allow,
                    "non-root must allow: initial={initial} escape={escape:?}"
                );
            }
        }
    }

    #[test]
    fn a_mapped_root_boot_allows_regardless_of_escape() {
        for escape in [None, Some(""), Some("0"), Some("1"), Some("true")] {
            assert_eq!(
                decide(true, false, escape),
                Decision::Allow,
                "nested-namespace root is not real root: escape={escape:?}"
            );
        }
    }

    #[test]
    fn real_root_without_the_exact_unlock_refuses() {
        for escape in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some(" 1"),
            Some("1 "),
        ] {
            assert!(
                matches!(decide(true, true, escape), Decision::Refuse(_)),
                "must refuse real root: escape={escape:?}"
            );
        }
    }

    #[test]
    fn real_root_with_the_exact_unlock_allows_unsafely() {
        assert_eq!(decide(true, true, Some("1")), Decision::AllowUnsafeRoot);
    }

    #[test]
    fn the_refusal_names_the_escape_and_the_recommended_shape() {
        let Decision::Refuse(reason) = decide(true, true, None) else {
            panic!("expected a refusal");
        };
        assert!(reason.contains(ACCEPT_UNSAFE_RUN_AS_ROOT), "{reason}");
        assert!(reason.contains("dedicated unprivileged user"), "{reason}");
    }

    // --- uid_map_is_initial: parser samples -------------------------------

    #[test]
    fn the_full_identity_map_is_initial() {
        assert!(uid_map_is_initial("0 0 4294967295"));
        assert!(uid_map_is_initial("0 0 4294967295\n"));
        assert!(uid_map_is_initial("  0 0 4294967295  "));
    }

    #[test]
    fn mapped_and_degenerate_maps_are_not_initial() {
        assert!(!uid_map_is_initial("0 1000 1"));
        assert!(!uid_map_is_initial("0 100000 65536"));
        assert!(!uid_map_is_initial("0 0 4294967295\n0 1000 1"));
        assert!(!uid_map_is_initial("garbage"));
        assert!(!uid_map_is_initial("0 0 4294967296"));
        assert!(!uid_map_is_initial("1 0 4294967295"));
        assert!(!uid_map_is_initial(""));
    }

    #[test]
    fn well_formed_maps_are_three_decimal_fields_per_line() {
        assert!(well_formed_uid_map("0 0 4294967295"));
        assert!(well_formed_uid_map("0 1000 1\n102 100000 65536\n"));
        assert!(!well_formed_uid_map("garbage"));
        assert!(!well_formed_uid_map("0 0"));
        assert!(!well_formed_uid_map("0 0 4294967295 extra"));
        assert!(!well_formed_uid_map("0 x 1"));
    }

    // --- live evidence ----------------------------------------------------

    #[test]
    fn the_fleet_sandbox_runs_inside_a_nested_userns() {
        // Case-2 live evidence: this host runs the suite as a mapped root,
        // so the guard's allow path is what a fleet boot takes. Irrelevant
        // for a non-root runner (the guard never probes there); on an
        // initial-namespace root host this failing IS the guard working.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let content = read_uid_map().expect("/proc/self/uid_map readable on Linux");
        assert!(
            !uid_map_is_initial(&content),
            "the sandbox became the initial namespace — a fleet tagma boot would now be refused"
        );
    }
}
