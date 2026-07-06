use std::sync::atomic::{AtomicU64, Ordering};

/// The apply integration test needs landlock kernel support (seccomp is layered
/// on by `landlock::apply`); skip when the kernel lacks it.
fn landlock_unsupported() -> bool {
    crate::landlock::ensure_supported().is_err()
}

/// A parent dir guaranteed outside the baseline writable set (`$TMPDIR`/`tmp`),
/// so a writable child of it is genuinely landlock-grantable — those temp roots
/// are symlinks on some systems, which landlock rejects for full access. Mirrors
/// the landlock tests' `non_baseline_parent`.
fn non_baseline_parent() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_TARGET_TMPDIR").map(std::path::PathBuf::from)
}

fn unique_dir(parent: &std::path::Path, label: &str) -> std::path::PathBuf {
    static M: AtomicU64 = AtomicU64::new(0);
    let n = M.fetch_add(1, Ordering::Relaxed);
    let p = parent.join(format!("ja-seccomp-{label}-{n}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

// The pure denylist-BPF-shape and EPERM-runtime-return probes now live in the
// libsandbox crate (`src/seccomp/`: `test_bpf_program_structure`,
// `test_deny_with_errno_compiles_bpf_errno_action`, and the `Errno(EPERM)`
// runtime test at `5e539d1`). What stays here is the **apply** integration test
// — it drives the real composition (`apply` wiring libsandbox primitives + our
// seccomp filter) end-to-end through a tokio spawn.

/// The denylist is a fixed set of `libc::SYS_*` constants, so its per-arch
/// resolvability is settled at compile time (a typo or arch-absent syscall is a
/// compile error — the crate won't build). What this no-kernel test pins down is
/// that the fixed denylist passes `SeccompFilterBuilder::build`'s validation —
/// i.e. it doesn't accidentally deny `exit`/`exit_group` or exceed the 255-rule
/// BPF jump limit.
#[test]
fn denylist_filter_builds() {
    // Forces the OnceLock get_or_init; `.build().expect(...)` inside would panic
    // here at test time if the denylist ever tripped the exit/overflow check.
    let _ = super::filter();
}

/// End-to-end via `landlock::apply`: the seccomp feature layers the filter onto
/// the real spawn path, so the child must report seccomp mode 2 (filter) in
/// `/proc/self/status`.
#[tokio::test]
async fn apply_installs_seccomp_filter() {
    if landlock_unsupported() {
        return;
    }
    // Writable dir OUTSIDE the baseline temp roots (see non_baseline_parent).
    let Some(parent) = non_baseline_parent() else {
        return;
    };
    let writable = unique_dir(&parent, "writable");
    let decision = crate::landlock::AccessDecision {
        read: crate::landlock::ReadPolicy::Broad,
        writable: vec![writable],
        readonly_holes: vec![],
        hide_holes: vec![],
    };
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg("grep 'Seccomp:' /proc/self/status")
        .kill_on_drop(true);
    crate::landlock::apply(&mut cmd, &decision).unwrap();
    let out = cmd.spawn().unwrap().wait_with_output().await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Seccomp:\t2"),
        "seccomp filter not active (expected Seccomp: 2); stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `mount(2)` is on the denylist: an attempted mount must fail (non-zero exit)
/// rather than succeed. The bash child holds CAP_SYS_ADMIN in its user ns, so
/// without the filter a tmpfs mount could succeed — this test pins that the
/// filter reclaims that capability. (Denylist membership itself is statically
/// pinned by the `libc::SYS_*` constant at compile time; this is the runtime
/// confirmation.)
#[tokio::test]
async fn mount_syscall_is_denied() {
    if landlock_unsupported() {
        return;
    }
    let Some(parent) = non_baseline_parent() else {
        return;
    };
    let writable = unique_dir(&parent, "writable");
    let mnt = writable.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let decision = crate::landlock::AccessDecision {
        read: crate::landlock::ReadPolicy::Broad,
        writable: vec![writable],
        readonly_holes: vec![],
        hide_holes: vec![],
    };
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "mount -t tmpfs tmpfs {} >/dev/null 2>&1; echo EXIT:$?",
            mnt.display()
        ))
        .kill_on_drop(true);
    crate::landlock::apply(&mut cmd, &decision).unwrap();
    let out = cmd.spawn().unwrap().wait_with_output().await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("EXIT:0"),
        "mount unexpectedly succeeded under the seccomp filter; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Positive smoke: a trivial command still runs under the filter (the denylist
/// does not break basic exec). `cargo`/`git` are avoided as probes — they may
/// re-exec via shims and produce env-specific noise; `echo` is deterministic.
#[tokio::test]
async fn trivial_command_runs_under_filter() {
    if landlock_unsupported() {
        return;
    }
    let Some(parent) = non_baseline_parent() else {
        return;
    };
    let writable = unique_dir(&parent, "writable");
    let decision = crate::landlock::AccessDecision {
        read: crate::landlock::ReadPolicy::Broad,
        writable: vec![writable],
        readonly_holes: vec![],
        hide_holes: vec![],
    };
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c").arg("echo ok").kill_on_drop(true);
    crate::landlock::apply(&mut cmd, &decision).unwrap();
    let out = cmd.spawn().unwrap().wait_with_output().await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ok") && out.status.success(),
        "trivial command failed under the seccomp filter; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
