//! landlock + mount-ns enforcement — compose the libsandbox primitives onto a
//! spawned process.
//!
//! A **general mechanism** (Linux-only, behind the `landlock` feature; **not** a
//! full sandbox — no network/resource isolation): given a
//! `tokio::process::Command` and an [`AccessDecision`](crate::landlock::AccessDecision), restrict the spawned
//! process so it may read per the decision's read policy, write only to the
//! listed dirs, and find other agents' locked workspaces carved read-only.
//!
//! This module is mechanism-only and knows nothing about backends or lock
//! managers. The heavy lifting — ruleset construction, `landlock_restrict_self`,
//! and the composable mount-ns primitives (user/mount-ns entry, directory
//! self-bind remount, tmpfs overlay) — lives in the `libsandbox` crate's
//! `prepare_*`/`install_*` pairs (`mount::child`). Callers compose them: the
//! shell backend
//! ([`ProcessBackend`](crate::backend::ProcessBackend) foreground commands and
//! the background supervisor) builds the writable list from the owning agent's
//! **current** directory write-locks (coordinated by the runtime crate's
//! directory-lock coordinator); the foreground path writes nothing on disk
//! except overflow spill files (under `temp_dir()`, already in
//! `baseline_writable`), and needs no scratch beyond `baseline_writable`, while
//! each background task adds its own per-spawn tmpdir as scratch. Then it calls
//! [`crate::landlock::apply`]. landlock turns that advisory lock decision into a mandatory one —
//! a process physically cannot write a directory not in its writable list.
//!
//! # Per-spawn snapshot
//!
//! The writable set is read fresh at each spawn (one `bash` per command), so
//! the domain always reflects the agent's locks *as of
//! that command*. See the plan's "Known limitations": a one-command overlap
//! window exists after release, and the snapshot is point-in-time.
//!
//! # Fail-closed
//!
//! Two gates ensure we never run `bash` unrestricted by accident:
//! 1. libsandbox's `prepare_landlock` builds the ruleset with
//!    `CompatLevel::HardRequirement` — if the kernel lacks landlock (or it is
//!    disabled at boot), ruleset creation errors and [`crate::landlock::apply`] returns `Err`,
//!    aborting the spawn before it starts.
//! 2. the `pre_exec` closure runs `install_user_mount_ns` → `install_bind` (×N)
//!    → `install_tmpfs` (×N) → `install_landlock` (→ seccomp when the `seccomp`
//!    feature is on) in the child; any step returning `Err` aborts the exec and
//!    fails the spawn.
//!
//! A full "write-outside-is-denied" assertion is covered by the test suite
//! rather than a runtime self-test (a robust runtime probe needs a path
//! guaranteed outside every baseline-writable dir, which is fragile to compute).

#![cfg(all(target_os = "linux", feature = "landlock"))]

use std::io;
use std::sync::OnceLock;

mod decision;

pub use decision::{AccessDecision, ReadPolicy};

/// System paths a Guest (narrow-read) `bash` needs to read+execute to function.
/// Delegated to libsandbox (the canonical list) so the two crates cannot drift;
/// the runtime composes it with the workspace to form the Guest read allowlist
/// (`.draft/design/agent-sandbox.md` §4.3).
pub fn baseline_readable() -> Vec<std::path::PathBuf> {
    libsandbox::landlock::baseline_readable()
}

/// Scratch/device paths every landlocked `bash` needs (redirects to `/dev/null`,
/// shell temp under `$TMPDIR`, ...). Delegated to libsandbox (the canonical list)
/// and folded into the writable set in [`apply`] — libsandbox takes `writable`
/// verbatim, so the baseline must be composed on this side. Asymmetric with
/// [`baseline_readable`], which the *caller* composes into a narrow-read allowlist.
pub fn baseline_writable() -> Vec<std::path::PathBuf> {
    libsandbox::landlock::baseline_writable()
}

/// Map kallip's [`ReadPolicy`] onto libsandbox's (the two are shape-identical:
/// `Broad` / `Narrow { paths }`).
fn map_read_policy(policy: &ReadPolicy) -> libsandbox::ReadPolicy {
    match policy {
        ReadPolicy::Broad => libsandbox::ReadPolicy::Broad,
        ReadPolicy::Narrow { paths } => libsandbox::ReadPolicy::Narrow {
            paths: paths.clone(),
        },
    }
}

/// Flatten a libsandbox error into a plain `io::Error` (carries its `Display`).
fn ioify(e: libsandbox::SandboxError) -> io::Error {
    io::Error::other(e.to_string())
}

/// Apply a landlock domain (+ mount-ns readonly/hide holes) to `cmd` per the
/// composed [`AccessDecision`].
///
/// Pure composition: parent-side `prepare_*` (may allocate) build the ruleset fd
/// and the prepared mount-ns data, then a `pre_exec` closure runs the child-side
/// `install_*` steps (raw syscalls) in the load-bearing order
///
/// ```text
/// user-mount-ns → bind(×N readonly) → tmpfs(×N hide) → landlock → seccomp
/// ```
///
/// so binds/tmpfs get `CAP_SYS_ADMIN` in the new userns, landlock resolves
/// against the post-overlay view, and seccomp is installed last (it must permit
/// `unshare`/`mount`/`open`/`write`/`close` during the earlier steps). The
/// `seccomp` step is kallip's own denylist **policy** (`crate::seccomp`'s
/// `BASE_DENY`/`X86_DENY`); its BPF is built and installed by libsandbox's
/// `SeccompFilterBuilder`/`SeccompFilter::install`.
///
/// A fresh user+mount namespace is entered iff there is any mount-layer work
/// (readonly holes for peer workspaces, hide holes for secrets); otherwise only
/// landlock (+ seccomp) applies. The prepared values are moved into the
/// `pre_exec` closure, which `cmd` owns until `spawn()` consumes it — so the
/// ruleset fd / mount data survive across the fork and are read in the child.
/// No guard needs to be held by the caller.
pub fn apply(cmd: &mut tokio::process::Command, decision: &AccessDecision) -> io::Result<()> {
    // Parent-side prepare (allocates CStrings / builds the ruleset fd).

    // Enter a fresh user + mount namespace iff there is any mount-layer work.
    // Both readonly holes (peer workspaces) and hide holes (secrets) are
    // mount-ns mechanisms landlock cannot express.
    let prepared_ns = if decision.readonly_holes.is_empty() && decision.hide_holes.is_empty() {
        None
    } else {
        // SAFETY: getuid/getgid take no args and cannot fail.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let ns = libsandbox::prepare_user_mount_ns(uid, gid);
        // Peer locked workspaces → read-only: self-bind + non-recursive RO
        // remount (the original readonly-hole semantics, which a recursive
        // remount would regress by also locking nested mounts).
        let binds = decision
            .readonly_holes
            .iter()
            .map(|p| {
                libsandbox::prepare_bind(
                    p,
                    libsandbox::Permission::ReadOnly,
                    libsandbox::RemountRecursion::NonRecursive,
                )
            })
            .collect::<libsandbox::Result<Vec<_>>>()
            .map_err(ioify)?;
        // Secret dirs → empty read-only tmpfs overlay (hide contents; size is
        // irrelevant for a read-only mount). ro + nodev + nosuid + noexec.
        let hides = decision
            .hide_holes
            .iter()
            .map(|p| {
                libsandbox::prepare_tmpfs(
                    p,
                    0,
                    libsandbox::MountFlags::READ_ONLY
                        | libsandbox::MountFlags::NO_EXEC
                        | libsandbox::MountFlags::NO_SUID
                        | libsandbox::MountFlags::NO_DEV,
                )
            })
            .collect::<libsandbox::Result<Vec<_>>>()
            .map_err(ioify)?;
        Some((ns, binds, hides))
    };

    // landlock: {read, writable} → libsandbox AccessDecision (which carries no
    // hole fields — those are mount-ns, realized above). libsandbox takes
    // `writable` verbatim, so fold in the canonical baseline (/dev/null, /tmp,
    // ...) every landlocked bash needs. Duplicates and missing paths are
    // harmless — libsandbox dedups and skips non-existent entries.
    let mut writable = decision.writable.clone();
    writable.extend(baseline_writable());
    let ll_decision = libsandbox::AccessDecision {
        read: map_read_policy(&decision.read),
        writable,
    };
    let prepared_ll = libsandbox::prepare_landlock(&ll_decision).map_err(ioify)?;

    // seccomp denylist (defense-in-depth): the cached libsandbox filter, installed
    // LAST — after the mount/landlock setup, so the filter cannot block the setup
    // syscalls themselves. `SeccompFilter::install` sets its own NO_NEW_PRIVS
    // (redundant with landlock's, harmless). Resolved parent-side; `filter()` is
    // &'static so its one-time get_or_init (which allocates) never runs in pre_exec.
    #[cfg(feature = "seccomp")]
    let seccomp_filter = crate::seccomp::filter();

    // SAFETY: the closure runs post-fork / pre-exec. On the success path it calls
    // only libsandbox's async-signal-safe install_* (raw syscalls, no allocation).
    // On an Err path the `.map_err(ioify)?` formatting allocates before the exec
    // is aborted — acceptable because the child is failing anyway. Order is
    // user-mount-ns → bind → tmpfs → landlock → seccomp; see above.
    unsafe {
        cmd.pre_exec(move || {
            if let Some((ns, binds, hides)) = &prepared_ns {
                libsandbox::install_user_mount_ns(ns).map_err(ioify)?;
                for b in binds {
                    libsandbox::install_bind(b).map_err(ioify)?;
                }
                for h in hides {
                    libsandbox::install_tmpfs(h).map_err(ioify)?;
                }
            }
            libsandbox::install_landlock(&prepared_ll).map_err(ioify)?;
            #[cfg(feature = "seccomp")]
            seccomp_filter.install().map_err(ioify)?;
            Ok(())
        });
    }
    Ok(())
}

/// Probe landlock support exactly once, process-wide. Delegates to libsandbox's
/// ruleset build (a trivial `prepare_landlock`): on an unsupported/disabled
/// kernel this errors and is cached so subsequent calls fail fast. Used by the
/// test suites' skip guards.
///
/// The underlying cause is preserved in the cached message (mirroring libsandbox's
/// own probe) so a fail-closed abort surfaces *why* — e.g. `ENOSYS` vs
/// `EOPNOTSUPP` — rather than a bare "landlock unavailable".
pub fn ensure_supported() -> io::Result<()> {
    // Stored as a `String` because `io::Error`/`SandboxError` are not `Clone`.
    static SUPPORT: OnceLock<Result<(), String>> = OnceLock::new();
    let cached = SUPPORT.get_or_init(|| {
        libsandbox::prepare_landlock(&libsandbox::AccessDecision {
            read: libsandbox::ReadPolicy::Broad,
            writable: Vec::new(),
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
    });
    match cached {
        Ok(()) => Ok(()),
        Err(msg) => Err(io::Error::other(format!("landlock unavailable: {msg}"))),
    }
}

#[cfg(test)]
mod tests;
