//! seccomp denylist enforcement (defense-in-depth on top of landlock).
//!
//! landlock + mount-ns restrict a spawned `bash`'s *filesystem* access. This
//! module adds a complementary, narrower defense: a seccomp filter that blocks a
//! small list of **high-risk syscalls an agent's shell will never legitimately
//! need** (`reboot`, `kexec_*`, `init_module`/`finit_module`/`delete_module`,
//! `bpf`, `perf_event_open`, `ptrace`, `kcmp`, `process_vm_readv/writev`,
//! `open_by_handle_at`, `swapon`/`swapoff`, …).
//!
//! # Denylist, not allowlist
//!
//! The filter is **default-allow**: every syscall passes except the named few.
//! This avoids the expensive, fragile curation of an allowlist (enumerating
//! exactly what bash + glibc + tokio need) — the false-positive surface of a
//! denylist of never-needed syscalls is essentially zero. A denied syscall
//! returns `EPERM` (not `SIGSYS`), so a misfire degrades a command rather than
//! killing the agent's whole session. A foreign-architecture syscall (a classic
//! compat-mode bypass) returns `KILL_PROCESS`.
//!
//! # Composed from libsandbox
//!
//! The filter is built with libsandbox's `SeccompFilterBuilder`
//! (`SeccompAction::Allow` default + `deny_with_errno(EPERM)` per syscall) and
//! installed in the child via `SeccompFilter::install`. The BPF emission and the
//! per-arch `AUDIT_ARCH` guard (foreign-arch → `KILL_PROCESS`) live in libsandbox;
//! this module owns only the **policy** — *which* syscalls an agent must never
//! invoke (the `BASE_DENY` / `X86_DENY` constants below).
//!
//! The denylist uses `libc::SYS_*` **compile-time constants** (`Syscall` is a
//! transparent alias for `libc::c_long`), so a typo or an unknown syscall is a
//! compile error, not a runtime panic.
//!
//! # Placement in `pre_exec`
//!
//! The filter is installed as the **last** step of the spawn path's `pre_exec`
//! closure — after landlock/mount-ns setup, so it cannot block the setup
//! syscalls themselves. `SeccompFilter::install` sets its own `PR_SET_NO_NEW_PRIVS`
//! (redundant with `install_landlock`'s, but harmless). Layered on by
//! `landlock::apply`.

#![cfg(all(target_os = "linux", feature = "seccomp"))]

use std::sync::OnceLock;

/// The default denylist: rare, high-impact syscalls an agent's bash/cargo/git
/// never legitimately invokes, **grouped by threat category** (see the inline
/// section comments for the why-is-this-denied AND why-is-this-safe rationale
/// per group). `libc::SYS_*` compile-time constants; entries absent on the
/// target arch live in [`X86_DENY`] (cfg-gated).
///
/// This is intentionally NOT an allowlist — adding a syscall is a deliberate
/// "this must never run" decision. When editing: only add syscalls bash/glibc/
/// ld.so NEVER call at startup. Do NOT add e.g. `uname`/`getrandom`/`clone` —
/// glibc/ld.so touch them and denying breaks every spawned bash.
const BASE_DENY: &[libc::c_long] = &[
    // Kernel reload & modules
    // Host self-sabotage; agent never needs.
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    // Cross-process inspection / injection
    // The only barrier to one agent reading another's memory (agents share
    // a uid; landlock is FS-only) — the threat is cross-agent, not self-use.
    // Cost: blocks gdb/strace/rr/valgrind attach; own backtrace unaffected.
    libc::SYS_ptrace,
    libc::SYS_kcmp,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    // eBPF / perf counters
    // Local-root + Spectre side-channel primitives; blocks perf/bpftrace.
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    // Filesystem-handle escape
    // open_by_handle_at bypasses path resolution — a chroot break-out.
    libc::SYS_open_by_handle_at,
    // Swap / accounting / kernel log
    // Host controls; syslog is man 2 (kernel dmesg), not libc syslog(3).
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_syslog,
    libc::SYS_acct,
    // Mount primitives
    // The sandbox runs bash in its own user+mount namespace, where it holds
    // CAP_SYS_ADMIN (needed to install the carve-out binds/remounts during
    // pre_exec, BEFORE this filter loads). Post-exec the shell has no legitimate
    // reason to mount/umount, so denying these reclaims that capability —
    // least-privilege. Concretely this closes a dirlock TOCTOU: without it, a
    // sandboxed agent could `umount2(MNT_DETACH)` a peer's readonly carve-out
    // bind (which makes the locked dir a mountpoint) and then `rename` it,
    // breaking the path-keyed lock. With mount/umount blocked, the mountpoint
    // stays and its rename hits EBUSY.
    // `rename`/`renameat`/`renameat2` are deliberately NOT denied — the agent
    // renames files constantly. The carve-out mountpoint blocks renames of the
    // *locked dir itself* (EBUSY) without touching ordinary file renames.
    // Diverges from libsandbox's BLOCKED_SYSCALLS, which also blocks
    // `unshare`/`setns`: left allowed here because landlock already covers their
    // filesystem effect and some dev tooling uses namespaces.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_open_tree,
    libc::SYS_move_mount,
    libc::SYS_mount_setattr,
];

/// x86-era legacy syscalls an agent never needs: `iopl`/`ioperm`/`uselib` are
/// absent on aarch64; `nfsservctl` is gated alongside as obsolete (it does exist
/// on aarch64 — conservative).
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
const X86_DENY: &[libc::c_long] = &[
    libc::SYS_iopl,
    libc::SYS_ioperm,
    libc::SYS_nfsservctl,
    libc::SYS_uselib,
];

/// The compiled default-denylist seccomp filter, cached process-wide.
///
/// Built once on first use and shared read-only thereafter: the stateless
/// backend spawns one `bash` per command, so rebuilding a constant filter per
/// spawn is pure waste. `SeccompFilter` holds the compiled BPF program and is
/// `Send + Sync`, safe to install concurrently from many spawn threads.
///
/// Call this **parent-side** (in `apply`, outside `pre_exec`) and capture the
/// returned `&'static` filter into the `pre_exec` closure. Never call it *inside*
/// the closure: the first invocation would run `get_or_init` (which allocates)
/// in async-signal context, which is forbidden there.
pub(crate) fn filter() -> &'static libsandbox::seccomp::SeccompFilter {
    static SECCOMP: OnceLock<libsandbox::seccomp::SeccompFilter> = OnceLock::new();
    SECCOMP.get_or_init(|| {
        // default-allow (denylist semantics); each denied syscall → ERRNO(EPERM).
        // `deny_with_errno` takes a `Syscall` (= `libc::c_long`) and returns
        // `Self` — no runtime name resolution, so no failure path here.
        let mut builder = libsandbox::seccomp::SeccompFilterBuilder::new(
            libsandbox::seccomp::SeccompAction::Allow,
        );
        for &nr in BASE_DENY {
            builder = builder.deny_with_errno(nr, libc::EPERM as u16);
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        for &nr in X86_DENY {
            builder = builder.deny_with_errno(nr, libc::EPERM as u16);
        }
        // `build` only fails on >255-rule overflow or denying `exit`/`exit_group`
        // under a non-callable default — neither applies to this fixed denylist.
        builder.build().expect("seccomp denylist compiles")
    })
}

#[cfg(test)]
mod tests;
