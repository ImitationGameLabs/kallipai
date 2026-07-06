//! Agent-agnostic per-spawn access decision consumed by landlock + mount-ns
//! enforcement.
//!
//! This is the **mechanism contract** at the seam between the runtime (which
//! owns agent policy — permission classes, tiers, the dirlock coordinator) and
//! the shell (which owns enforcement). It deliberately carries no agent
//! identity, tier, or policy labels: the shell crate stays decoupled from the
//! runtime, and the runtime maps its policy onto these mechanism types when it
//! builds the snapshot closure (`.draft/design/agent-sandbox.md` §6.1).
//!
//! Effective access = baseline (read policy + writable set) ∩ dirlock overlay
//! (readonly holes) ∩ secret hide-holes (tmpfs-over overlays). See §2.3 "正交叠加".

#![cfg(all(target_os = "linux", feature = "landlock"))]

use std::path::PathBuf;

/// Read policy for a spawned process — the difference between the Normal (broad)
/// and Guest (broad + secret hide-holes) recipes.
#[derive(Clone)]
pub enum ReadPolicy {
    /// Broad read+exec on `/`: bash and its libs load normally and the agent can
    /// read source/caches anywhere (e.g. `~/.cargo`). Secrets under `~/.ssh` etc.
    /// are readable unless carved out by [`AccessDecision::hide_holes`] (the
    /// Guest recipe) or mitigated by proxy tools (the Normal recipe).
    Broad,
    /// Narrow read: only `paths` are granted read; everything else — including
    /// `$HOME` and secrets — is denied by default (landlock's `handle_access(full)`
    /// is deny-default, so anything not listed is unreadable). `paths` must
    /// include enough for bash/libs to run (e.g. `/usr`, `/bin`, `/lib`) plus the
    /// workspace. Currently unused by either permission class (both are `Broad`);
    /// retained for a future even-more-restricted recipe.
    Narrow { paths: Vec<PathBuf> },
}

/// Per-spawn access decision, composed by the runtime closure and consumed by
/// [`crate::landlock::apply`].
///
/// - `writable` is granted write (the agent's write-locks + scratch + baseline
///   temp/devices). The baseline is folded in by [`super::apply`] (asymmetric
///   with baseline-readable, which the *caller* composes for a narrow-read
///   allowlist). For the readonly recipe this is typically just the skills
///   carve + scratch.
/// - `readonly_holes` are other agents' locked workspaces — bind-mounted
///   read-only by the mount-ns layer so a broad-write agent cannot mutate a
///   workspace someone else holds. Empty for a writer that holds the lock on
///   every path it touches. landlock provably cannot express these holes inside
///   a writable tree (`writable_ancestor_cannot_be_narrowed_to_readonly`), so
///   they are realized by mount-ns (self-bind + non-recursive read-only remount),
///   with landlock applied *after* the remount.
/// - `hide_holes` are secret directories a broad-read agent must not see —
///   overlaid by an empty read-only tmpfs (mount-ns) so the real contents are
///   invisible while the rest of the tree stays readable. landlock cannot carve
///   a deny out of a broad-read tree (the same no-subtraction asymmetry), so this
///   too is a mount-ns mechanism, distinct from `readonly_holes` (which makes a
///   writable path read-only, not hidden). Used by the Guest recipe.
///
/// **Invariant:** `readonly_holes` and `hide_holes` must be **prefix-disjoint**
/// (no entry is a prefix of another — including the degenerate equal-path case).
/// Both are realized in the same mount namespace by [`crate::landlock::apply`]
/// (binds first, then tmpfs overlays); at a shared prefix the later tmpfs mount
/// wins by mount-stacking order, which would silently shadow a readonly-hole
/// bind. Equal paths are the boundary of this — tmpfs overlays the bind entirely.
/// The runtime keeps them disjoint by construction (peer workspaces vs home-dir
/// secrets), but the contract is documented here rather than enforced; the
/// `combo_*` tests in `landlock/tests.rs` characterize the actual stacking
/// winner for each overlap direction.
pub struct AccessDecision {
    pub read: ReadPolicy,
    pub writable: Vec<PathBuf>,
    pub readonly_holes: Vec<PathBuf>,
    pub hide_holes: Vec<PathBuf>,
}
