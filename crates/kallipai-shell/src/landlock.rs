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
//! that command*. Known limitation: a one-command overlap
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
//!    feature is on) in the child; any step returning `Err` aborts the exec,
//!    records the step and the path it refused in the `SpawnTrace` channel, and
//!    fails the spawn with that record attached.
//!
//! A full "write-outside-is-denied" assertion is covered by the test suite
//! rather than a runtime self-test (a robust runtime probe needs a path
//! guaranteed outside every baseline-writable dir, which is fragile to compute).

#![cfg(all(target_os = "linux", feature = "landlock"))]

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::error::{ShellError, SpawnStage};
use std::sync::OnceLock;

mod decision;

pub use decision::{AccessDecision, ReadPolicy};

/// System paths a Guest (narrow-read) `bash` needs to read+execute to function.
/// Delegated to libsandbox (the canonical list) so the two crates cannot drift;
/// the runtime composes it with the workspace to form the Guest read allowlist.
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

/// Map kallipai's [`ReadPolicy`] onto libsandbox's (the two are shape-identical:
/// `Broad` / `Narrow { paths }`).
fn map_read_policy(policy: &ReadPolicy) -> libsandbox::ReadPolicy {
    match policy {
        ReadPolicy::Broad => libsandbox::ReadPolicy::Broad,
        ReadPolicy::Narrow { paths } => libsandbox::ReadPolicy::Narrow {
            paths: paths.clone(),
        },
    }
}

/// The most detail bytes one frame carries, so one frame stays one small write.
///
/// A stream socket grants no message boundaries; what keeps the frame whole is
/// one writer, one frame and an empty socket at the write, plus the reader
/// clamping the declared length to the bytes it received. A short write (the
/// send buffer is fresh and never full here, so it would take an extraordinary
/// failure) shortens the frame from the tail: the header and the path index
/// arrive first, the detail is cut, and a frame shorter than the header is no
/// frame at all — the reader falls back instead of misreading.
const DETAIL_LIMIT: usize = 120;

/// Bytes before the detail: step code (1), path index (2, little endian),
/// detail length (2, little endian).
const FRAME_HEADER: usize = 5;

/// The out-of-band channel a sandboxed child records its refusal in.
///
/// # Why a channel and not the error value
///
/// Only one `i32` errno crosses `fork`: an `io::Error` a `pre_exec` closure
/// returns arrives in the parent as `Invalid argument (os error 22)` with its
/// payload gone, and the child's heap is a copy-on-write view of the parent's,
/// so a store through a shared `Arc` is invisible to the parent as well.
/// Kernel-mediated state is what survives: the child
/// therefore writes its refusal into a socketpair created before the fork, and
/// the parent reads that socket after `spawn` fails.
///
/// # What crosses
///
/// One frame, one `send`: step code, path index, detail length, detail bytes.
/// Exactly the five child-side install steps use it — user-mount-ns, bind,
/// tmpfs, landlock, seccomp — and nothing else does. Mount-ns and seccomp name
/// no path (`index = 0`); the landlock step sends stage code 0 and still sends
/// its detail, so the parent reports it as [`ShellError::Landlock`]: a ruleset
/// that could not be installed filtered nothing, which is not a spawn step that
/// refused.
///
/// # Bounds
///
/// Path index: `1..=u16::MAX`, an index that does not fit degrades to "step
/// named, no path". Detail: truncated to 120 bytes on a character
/// boundary. A truncated index, a length past the frame, an unknown step code
/// or an entry recorded for another step all degrade the same way — never a
/// neighbour's path, because a wrong path in an error message is worse than no
/// path at all.
///
/// Callers outside this crate hold it as an opaque token: hand it back after
/// `spawn` returns and let it drop. Reading the record is crate-internal,
/// because every spawn point that can produce one lives in this crate.
#[must_use = "hold the trace until `spawn` returns, then drop it"]
pub struct SpawnTrace {
    receiver: UnixStream,
    paths: Vec<(SpawnStage, PathBuf)>,
}

impl SpawnTrace {
    /// Creates the channel and the empty table: the returned trace stays with
    /// the caller, the sender goes into the `pre_exec` closure.
    ///
    /// Both ends carry `SOCK_CLOEXEC`, so a sandboxed process never inherits
    /// the channel: the child writes before its `exec`, the descriptor closes
    /// at `exec`, and the parent drops its read end together with the trace.
    fn new(paths: Vec<(SpawnStage, PathBuf)>) -> io::Result<(Self, UnixStream)> {
        let (receiver, sender) = UnixStream::pair()?;
        Ok((Self { receiver, paths }, sender))
    }

    /// The refusal the child recorded, or `None` when it recorded nothing.
    ///
    /// One non-blocking `recv`, only ever on the failure path: by the time
    /// `spawn` returns an error the child has written its frame (or died
    /// without reaching the closure, e.g. `ENOENT`), so an empty read means
    /// "no record" and the caller falls back to what the parent can verify by
    /// itself.
    ///
    /// The frame comes from our own child and is still parsed defensively: see
    /// the type-level notes on bounds. Nothing here panics, and no entry is
    /// ever substituted for another.
    pub(crate) fn refused(&self) -> Option<crate::error::RecordedRefusal> {
        let mut frame = [0u8; FRAME_HEADER + DETAIL_LIMIT];
        let received = nix::sys::socket::recv(
            self.receiver.as_raw_fd(),
            &mut frame,
            nix::sys::socket::MsgFlags::MSG_DONTWAIT,
        )
        .ok()?;
        if received < FRAME_HEADER {
            return None;
        }
        let stage = SpawnStage::from_code(frame[0]);
        let index = u16::from_le_bytes([frame[1], frame[2]]);
        let declared = usize::from(u16::from_le_bytes([frame[3], frame[4]]));
        let length = declared.min(received - FRAME_HEADER);
        let detail =
            String::from_utf8_lossy(&frame[FRAME_HEADER..FRAME_HEADER + length]).into_owned();
        let path = usize::from(index)
            .checked_sub(1)
            .and_then(|position| self.paths.get(position))
            .filter(|(entry_stage, _)| Some(*entry_stage) == stage)
            .map(|(_, path)| path.clone());
        Some((stage, path, detail))
    }
}

/// Records the refusal in `channel` and returns the errno the `pre_exec`
/// closure must fail with.
///
/// Allocation-free by design: after `fork` the child shares the parent's
/// allocator state, so it formats nothing. It copies the step code, an index
/// and at most [`DETAIL_LIMIT`] bytes of the already-formatted context into one
/// stack frame and issues a single `send`. The returned errno is a formality —
/// `libsandbox::SandboxError` keeps no errno, and `fork` carries nothing else —
/// so it stays `EINVAL`, the value `std` substitutes when a child error has no
/// errno of its own; the parent reads the frame instead.
fn refuse(
    channel: &UnixStream,
    stage: Option<SpawnStage>,
    path: Option<u16>,
    detail: &str,
) -> io::Error {
    let mut frame = [0u8; FRAME_HEADER + DETAIL_LIMIT];
    frame[0] = stage.and_then(SpawnStage::code).unwrap_or(0);
    frame[1..3].copy_from_slice(&path.unwrap_or(0).to_le_bytes());
    let mut length = detail.len().min(DETAIL_LIMIT);
    while !detail.is_char_boundary(length) {
        length -= 1;
    }
    frame[3..5].copy_from_slice(&u16::try_from(length).unwrap_or(0).to_le_bytes());
    frame[FRAME_HEADER..FRAME_HEADER + length].copy_from_slice(&detail.as_bytes()[..length]);
    let _ = nix::sys::socket::send(
        channel.as_raw_fd(),
        &frame[..FRAME_HEADER + length],
        nix::sys::socket::MsgFlags::MSG_NOSIGNAL,
    );
    io::Error::from_raw_os_error(libc::EINVAL)
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
/// `seccomp` step is kallipai's own denylist **policy** (`crate::seccomp`'s
/// `BASE_DENY`/`X86_DENY`); its BPF is built and installed by libsandbox's
/// `SeccompFilterBuilder`/`SeccompFilter::install`.
///
/// A fresh user+mount namespace is entered iff there is any mount-layer work
/// (readonly holes for peer workspaces, hide holes for secrets); otherwise only
/// landlock (+ seccomp) applies. The prepared values are moved into the
/// `pre_exec` closure, which `cmd` owns until `spawn()` consumes it — so the
/// ruleset fd / mount data survive across the fork and are read in the child.
/// No guard needs to be held by the caller.
///
/// Parent-side preparation fails here, before any fork, and returns the
/// structured error directly. Child-side installation cannot: the child can
/// only send an errno back, so each step records itself in the returned
/// [`SpawnTrace`] instead, and the caller hands that trace to the spawn error.
pub fn apply(
    cmd: &mut tokio::process::Command,
    decision: &AccessDecision,
) -> Result<SpawnTrace, ShellError> {
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
        // Bind-mount plan, composed from two sources:
        //   * readonly holes (peers' locked workspaces) → read-only self-bind +
        //     non-recursive RO remount;
        //   * any of THIS agent's writable paths that nests under one of those
        //     holes → a read-write self-bind so the nested carve (a subagent's
        //     own workspace under its supervisor's locked root) stays writable.
        //
        // Without the writable child overlay, the ro self-bind on the ancestor
        // (recursive bind, non-recursive remount) makes the whole subtree
        // read-only at the VFS layer, and landlock's write grant cannot help —
        // the documented libsandbox invariant is "install writable children
        // BEFORE their read-only ancestors" so the ancestor's non-recursive RO
        // remount preserves the nested child mount's writability. Sort the merged
        // list descendant-first (more path components first): a strict descendant
        // has strictly more components than its ancestor, so this puts every child
        // before every ancestor it nests under; ties can't be prefix-related.
        let mut bind_specs: Vec<(std::path::PathBuf, libsandbox::Permission)> = Vec::new();
        for w in &decision.writable {
            if decision
                .readonly_holes
                .iter()
                .any(|hole| is_strict_descendant(w, hole))
            {
                bind_specs.push((w.clone(), libsandbox::Permission::ReadWrite));
            }
        }
        for hole in &decision.readonly_holes {
            bind_specs.push((hole.clone(), libsandbox::Permission::ReadOnly));
        }
        // Defense-in-depth at this security boundary: a future caller could push
        // a duplicate writable path. Collapse identical paths (a path always
        // carries one permission in the construction above) before sorting.
        bind_specs.sort_by(|(a, _), (b, _)| a.cmp(b));
        bind_specs.dedup_by(|(a, _), (b, _)| a == b);
        // Descendant-first: deepest paths install BEFORE their read-only
        // ancestors, so the ancestor's non-recursive RO remount preserves a
        // nested writable child mount's writability — the invariant pinned by
        // libsandbox's `readonly_ancestor_with_writable_child_overlay` test.
        // Flipping this to ancestor-first silently revokes child writability.
        // A strict descendant has strictly more path components than its
        // ancestor, so descending component-count puts every child first; the
        // path tie-break is cosmetic (equal-depth entries are prefix-disjoint).
        bind_specs.sort_by(|(a, _), (b, _)| {
            b.components()
                .count()
                .cmp(&a.components().count())
                .then_with(|| a.cmp(b))
        });
        // Paired plans: each prepared value travels with the path it came from,
        // so the install loop below cannot slide against its names.
        let binds = bind_specs
            .into_iter()
            .map(|(path, permission)| {
                libsandbox::prepare_bind(
                    &path,
                    permission,
                    libsandbox::RemountRecursion::NonRecursive,
                )
                .map(|prepared| (prepared, path.clone()))
                .map_err(|error| ShellError::SpawnFailed {
                    stage: SpawnStage::Bind,
                    path: Some(path),
                    detail: error.to_string(),
                })
            })
            .collect::<Result<Vec<_>, ShellError>>()?;
        // Secret dirs → empty read-only tmpfs overlay (hide contents; size is
        // irrelevant for a read-only mount). ro + nodev + nosuid + noexec.
        let hides = decision
            .hide_holes
            .iter()
            .map(|path| {
                libsandbox::prepare_tmpfs(
                    path,
                    0,
                    libsandbox::MountFlags::READ_ONLY
                        | libsandbox::MountFlags::NO_EXEC
                        | libsandbox::MountFlags::NO_SUID
                        | libsandbox::MountFlags::NO_DEV,
                )
                .map(|prepared| (prepared, path.clone()))
                .map_err(|error| ShellError::SpawnFailed {
                    stage: SpawnStage::Tmpfs,
                    path: Some(path.clone()),
                    detail: error.to_string(),
                })
            })
            .collect::<Result<Vec<_>, ShellError>>()?;
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
    // The ruleset itself is missing here, before any fork: no spawn step ran,
    // so this is `Landlock` and not a `SpawnFailed`.
    let prepared_ll =
        libsandbox::prepare_landlock(&ll_decision).map_err(|error| ShellError::Landlock {
            detail: error.to_string(),
        })?;

    // seccomp denylist (defense-in-depth): the cached libsandbox filter, installed
    // LAST — after the mount/landlock setup, so the filter cannot block the setup
    // syscalls themselves. `SeccompFilter::install` sets its own NO_NEW_PRIVS
    // (redundant with landlock's, harmless). Resolved parent-side; `filter()` is
    // &'static so its one-time get_or_init (which allocates) never runs in pre_exec.
    #[cfg(feature = "seccomp")]
    let seccomp_filter = crate::seccomp::filter();

    // The channel's path table, in the order the loops below install: bind
    // entries first, then tmpfs entries. A frame carries an index into this
    // very vector, so the table and the indexes come from one place.
    let mut paths = Vec::new();
    if let Some((_, binds, hides)) = &prepared_ns {
        paths.extend(
            binds
                .iter()
                .map(|(_, path)| (SpawnStage::Bind, path.clone())),
        );
        paths.extend(
            hides
                .iter()
                .map(|(_, path)| (SpawnStage::Tmpfs, path.clone())),
        );
    }
    let (trace, channel) = SpawnTrace::new(paths)?;
    let bind_count = prepared_ns.as_ref().map_or(0, |(_, binds, _)| binds.len());

    // SAFETY: the closure runs post-fork / pre-exec. On the success path it calls
    // only libsandbox's async-signal-safe install_* (raw syscalls; they allocate
    // only on their own failure path).
    // On an Err path this side formats nothing and allocates nothing: it copies
    // the step code, an index and the error's already-built context into a
    // pre-sized stack frame, issues one `send`, and returns a bare errno.
    // libsandbox's own failure path is what builds that context, inside the
    // child, with a `format!` its source marks as failure-path-only. Order is
    // user-mount-ns → bind → tmpfs → landlock → seccomp; see above.
    unsafe {
        cmd.pre_exec(move || {
            if let Some((ns, binds, hides)) = &prepared_ns {
                libsandbox::install_user_mount_ns(ns).map_err(|error| {
                    refuse(
                        &channel,
                        Some(SpawnStage::MountNamespace),
                        None,
                        error.context(),
                    )
                })?;
                for (position, (bind, _)) in binds.iter().enumerate() {
                    libsandbox::install_bind(bind).map_err(|error| {
                        refuse(
                            &channel,
                            Some(SpawnStage::Bind),
                            u16::try_from(position + 1).ok(),
                            error.context(),
                        )
                    })?;
                }
                for (position, (hide, _)) in hides.iter().enumerate() {
                    libsandbox::install_tmpfs(hide).map_err(|error| {
                        refuse(
                            &channel,
                            Some(SpawnStage::Tmpfs),
                            u16::try_from(bind_count + position + 1).ok(),
                            error.context(),
                        )
                    })?;
                }
            }
            // No step code: a ruleset that never took effect is its own error,
            // not a spawn step that refused.
            libsandbox::install_landlock(&prepared_ll)
                .map_err(|error| refuse(&channel, None, None, error.context()))?;
            #[cfg(feature = "seccomp")]
            seccomp_filter.install().map_err(|error| {
                refuse(&channel, Some(SpawnStage::Seccomp), None, error.context())
            })?;
            Ok(())
        });
    }
    Ok(trace)
}

/// Component-wise strict descendant test: `child != ancestor` and `child` starts
/// with `ancestor` as a path prefix. `Path::starts_with` is component-wise on
/// Unix, so `/a/b` does not match `/a/bb` (the same property the dirlock overlap
/// check leans on). Used in [`apply`] to decide which writable paths need a
/// mount-layer overlay to escape a read-only ancestor bind. The `!=` guard
/// defends equality, which is impossible today by construction (dirlock excludes
/// an agent's own locks from its `readonly_paths`) but cheap to keep as
/// defense-in-depth against a future caller.
fn is_strict_descendant(child: &std::path::Path, ancestor: &std::path::Path) -> bool {
    child != ancestor && child.starts_with(ancestor)
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
