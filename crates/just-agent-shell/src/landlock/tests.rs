use super::*;
use std::path::{Path, PathBuf};

/// landlock may be unavailable (old kernel, CI without it, boot-disabled); a
/// test calls this and returns early (skip) when so, rather than failing.
fn skip_if_unsupported() -> bool {
    ensure_supported().is_err()
}

/// `unshare(CLONE_NEWUSER)` unavailable on this kernel/boot (the sysctl is
/// `0`) or the sysctl absent on a kernel that disallows it. We skip the
/// mount-ns readonly-hole test when so, rather than failing — the test
/// environment may not permit unprivileged user namespaces.
fn userns_unavailable() -> bool {
    match std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        Ok(s) => s.trim() == "0",
        // Absent → assume permitted (some distros omit the sysctl entirely).
        Err(_) => false,
    }
}

/// A parent dir guaranteed *outside* the baseline writable set (`$TMPDIR` /
/// `/tmp` / `/var/tmp`), so a child path of it is genuinely write-denied by
/// landlock and not merely unwritable on the host. `CARGO_TARGET_TMPDIR`
/// (`<repo>/target/tmp`) is set by `cargo test` and is distinct from
/// [`std::env::temp_dir`].
fn non_baseline_parent() -> Option<PathBuf> {
    std::env::var_os("CARGO_TARGET_TMPDIR").map(PathBuf::from)
}

fn unique_dir(parent: &Path, label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = parent.join(format!("ja-landlock-{label}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The pure landlock ruleset / mount-hole / "rules only grant" limitation probes
/// now live in the libsandbox crate (`src/landlock/tests.rs`, `src/mount/holes.rs`).
/// What stays here are the **apply** integration tests — they drive the real
/// composition (`apply` wiring libsandbox primitives + our seccomp) end-to-end
/// through a tokio spawn.

#[tokio::test]
async fn write_outside_writable_is_denied() {
    if skip_if_unsupported() {
        return;
    }
    let Some(parent) = non_baseline_parent() else {
        return;
    };
    let allowed = unique_dir(&parent, "allowed");
    let outside = unique_dir(&parent, "outside"); // host-writable, NOT in writable set

    let target = outside.join("f");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!("echo x > '{}'", target.display()))
        .kill_on_drop(true);
    apply(
        &mut cmd,
        &AccessDecision {
            read: ReadPolicy::Broad,
            writable: vec![allowed.clone()],
            readonly_holes: vec![],
            hide_holes: vec![],
        },
    )
    .unwrap();
    let output = cmd.spawn().unwrap().wait_with_output().await.unwrap();

    // Without landlock the write would succeed (outside is host-writable);
    // with landlock it must be denied (EACCES → non-zero exit).
    assert!(
        !output.status.success(),
        "write outside writable unexpectedly succeeded; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !target.exists(),
        "the denied write nonetheless created the file"
    );
}

/// Baseline device/temp paths (`/dev/null`, `$TMPDIR`, ...) must stay writable
/// under landlock even with an empty `writable` set — every `bash` touches them
/// (the per-call wrapper redirects `2>/dev/null`, shells temp into `$TMPDIR`).
/// `apply` folds `baseline_writable()` into the set; pre-fix `/dev/null` was
/// absent and `2>/dev/null` hit `EACCES` on every spawn.
#[tokio::test]
async fn baseline_paths_writable_under_landlock() {
    if skip_if_unsupported() {
        return;
    }
    // Empty writable: only the folded baseline is writable. The primary guard
    // (/dev/null + $TMPDIR) needs no non-baseline parent, so it runs regardless
    // of `CARGO_TARGET_TMPDIR` (cargo sets it for `cargo test`, but not every
    // harness). The over-grant guard below is the only part that needs a
    // genuinely non-baseline path.
    let tmp = std::env::temp_dir();
    let tmp_target = tmp.join(format!("ja-baseline-writable-{}", std::process::id()));
    let denied_target = non_baseline_parent().map(|p| unique_dir(&p, "denied").join("f"));
    let denied_clause = match &denied_target {
        Some(d) => format!("; echo z > '{}'; echo \"denied_rc=$?\"", d.display()),
        None => String::new(),
    };
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "echo x > /dev/null 2>/dev/null; echo \"null_rc=$?\"; \
             echo y > '{tmp}'; echo \"tmp_rc=$?\"{denied}",
            tmp = tmp_target.display(),
            denied = denied_clause,
        ))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    apply(
        &mut cmd,
        &AccessDecision {
            read: ReadPolicy::Broad,
            writable: vec![],
            readonly_holes: vec![],
            hide_holes: vec![],
        },
    )
    .unwrap();
    let output = cmd.spawn().unwrap().wait_with_output().await.unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("null_rc=0"),
        "/dev/null write denied under landlock (baseline not folded?); \
         stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("tmp_rc=0"),
        "$TMPDIR write denied under landlock (baseline not folded?); \
         stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Over-grant guard: folding baseline must not make a non-baseline, non-
    // writable path writable. Ground-truth is the file's absence (matches
    // `write_outside_writable_is_denied`); the rc is a secondary signal.
    if let Some(denied_path) = denied_target.as_ref() {
        assert!(
            !stdout.contains("denied_rc=0"),
            "non-baseline write unexpectedly succeeded (over-grant?); stdout={stdout}"
        );
        assert!(
            !denied_path.exists(),
            "non-baseline write created a file (over-grant?); stdout={stdout}"
        );
    }
    let _ = std::fs::remove_file(&tmp_target);
}

#[tokio::test]
async fn write_inside_writable_succeeds() {
    if skip_if_unsupported() {
        return;
    }
    let Some(parent) = non_baseline_parent() else {
        return;
    };
    let allowed = unique_dir(&parent, "allowed");

    let target = allowed.join("f");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!("echo x > '{}'", target.display()))
        .kill_on_drop(true);
    apply(
        &mut cmd,
        &AccessDecision {
            read: ReadPolicy::Broad,
            writable: vec![allowed.clone()],
            readonly_holes: vec![],
            hide_holes: vec![],
        },
    )
    .unwrap();
    let status = cmd.spawn().unwrap().wait().await.unwrap();

    assert!(status.success(), "write inside writable failed");
    assert!(target.exists(), "the allowed write did not create the file");
}

/// **Mount-ns readonly hole** (§4.2): when the hole sits *under* a writable
/// ancestor (the Normal broad-write case), landlock alone CANNOT block it —
/// granting write on the ancestor covers the hole. The mount-ns layer (via
/// libsandbox's `prepare_mount_holes`/`install_mount_holes`) bind+remount-ro's
/// the hole, so the write is blocked by the ro mount regardless of landlock.
/// This is the load-bearing test for the DirLock reader view.
#[tokio::test]
async fn readonly_hole_blocks_write_under_writable_ancestor() {
    if skip_if_unsupported() || userns_unavailable() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let parent = unique_dir(&base, "mntparent");
    let hole = unique_dir(&parent, "hole");
    let sibling = unique_dir(&parent, "sibling");

    // Grant write on `parent` (covers both hole and sibling per landlock).
    // The hole is ALSO a readonly_hole → mount-ns bind+remount-ro. landlock
    // would permit the hole write; only the ro mount blocks it.
    let decision = AccessDecision {
        read: ReadPolicy::Broad,
        writable: vec![parent.clone()],
        readonly_holes: vec![hole.clone()],
        hide_holes: vec![],
    };

    let hole_target = hole.join("f");
    let sib_target = sibling.join("f");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "echo x > '{}' 2>/dev/null; echo \"hole_rc=$?\"; \
             echo x > '{}'; echo \"sib_rc=$?\"",
            hole_target.display(),
            sib_target.display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The sibling write must succeed — this also confirms bash actually ran
    // (guards against a false pass if unshare/exec silently failed).
    assert!(
        stdout.contains("sib_rc=0"),
        "sibling write failed unexpectedly (bash may not have run); \
         stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The hole write must be blocked by the ro mount (not hole_rc=0).
    assert!(
        !stdout.contains("hole_rc=0"),
        "hole write unexpectedly succeeded under a writable ancestor \
         (mount-ns ro did not block); stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !hole_target.exists(),
        "the blocked hole write nonetheless created a file"
    );
}

/// **Multi-level carve-out**: two NESTED readonly holes under one writable
/// ancestor (the delegation case — a grandchild's lock carves a hole inside an
/// already-carved child region). `readonly_paths` yields holes in `BTreeMap`
/// path order, not by depth; this test passes them DEEP-FIRST (the reverse
/// ordering) to confirm non-recursive remounts make each hole independently
/// read-only regardless of install order, while the non-hole sibling stays
/// writable. Pins an invariant the delegation carve-out relies on at depth ≥ 2.
#[tokio::test]
async fn nested_readonly_holes_both_block_under_writable_ancestor() {
    if skip_if_unsupported() || userns_unavailable() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let parent = unique_dir(&base, "mlparent");
    let h1 = unique_dir(&parent, "h1");
    let deep = unique_dir(&h1, "deep");
    let sib = unique_dir(&parent, "sib");

    // Holes deliberately deep-first (reverse of BTreeMap path-sort).
    let decision = AccessDecision {
        read: ReadPolicy::Broad,
        writable: vec![parent.clone()],
        readonly_holes: vec![deep.clone(), h1.clone()],
        hide_holes: vec![],
    };

    let deep_target = deep.join("f");
    let h1_target = h1.join("f");
    let sib_target = sib.join("f");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "echo x > '{}' 2>/dev/null; echo \"deep_rc=$?\"; \
             echo x > '{}' 2>/dev/null; echo \"h1_rc=$?\"; \
             echo x > '{}'; echo \"sib_rc=$?\"",
            deep_target.display(),
            h1_target.display(),
            sib_target.display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("sib_rc=0"),
        "sibling write failed unexpectedly; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("deep_rc=0"),
        "deep hole write unexpectedly succeeded; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("h1_rc=0"),
        "h1 hole write unexpectedly succeeded; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// **Narrow read**: only the listed `paths` are granted read; anything else
/// (here a "secret" dir outside both) is denied by default. This pins the Narrow
/// mechanism (landlock deny-default) — no longer the Guest recipe (Guest is now
/// broad-read + hide-holes, see `broad_read_with_hide_hole_hides_secret`), but
/// the variant is retained for a future even-more-restricted recipe.
#[tokio::test]
async fn narrow_read_denies_paths_outside_allowlist() {
    if skip_if_unsupported() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let workspace = unique_dir(&base, "p0ws");
    let secret = unique_dir(&base, "p0secret"); // outside workspace + baseline_readable
    std::fs::write(workspace.join("readable"), b"ok").unwrap();
    std::fs::write(secret.join("secret"), b"key").unwrap();

    let mut paths = vec![workspace.clone()];
    paths.extend(baseline_readable());
    let decision = AccessDecision {
        read: ReadPolicy::Narrow { paths },
        writable: vec![workspace.clone()],
        readonly_holes: vec![],
        hide_holes: vec![],
    };

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "cat '{}' 2>/dev/null; echo \"ws_rc=$?\"; \
             cat '{}' 2>/dev/null; echo \"secret_rc=$?\"",
            workspace.join("readable").display(),
            secret.join("secret").display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The workspace (in the allowlist) is readable; the secret (not in it) is
    // denied by default — the zero-access-to-unlisted property.
    assert!(
        stdout.contains("ws_rc=0"),
        "workspace read failed under narrow read (allowlist too tight?); stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("secret_rc=0"),
        "secret read unexpectedly succeeded under narrow read; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// **Guest recipe** (§4.3, post-correction): broad read + a secret hide-hole.
/// The agent can read anywhere (here a "cargo cache" dir, simulating `~/.cargo`)
/// but the hide-hole dir is overlaid by an empty tmpfs — its real contents are
/// invisible. This is the §2.1 conflict-b resolution for broad-read review/
/// research subagents: read source/caches, never the keys.
#[tokio::test]
async fn broad_read_with_hide_hole_hides_secret() {
    if skip_if_unsupported() || userns_unavailable() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let cargo = unique_dir(&base, "cargo"); // readable cache (broad read)
    let secret = unique_dir(&base, "secret"); // overlaid → contents hidden
    std::fs::write(cargo.join("crate"), b"src").unwrap();
    std::fs::write(secret.join("key"), b"topsecret").unwrap();

    let decision = AccessDecision {
        read: ReadPolicy::Broad,
        writable: vec![],
        readonly_holes: vec![],
        hide_holes: vec![secret.clone()],
    };

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "cat '{}' 2>/dev/null; echo \"cargo_rc=$?\"; \
             cat '{}' 2>/dev/null; echo \"key_rc=$?\"; \
             echo \"entries=$(ls -A '{}' 2>/dev/null | wc -l)\"",
            cargo.join("crate").display(),
            secret.join("key").display(),
            secret.display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Broad read reaches the cache; the hide-hole's real contents are invisible.
    assert!(
        stdout.contains("cargo_rc=0"),
        "cargo read failed under broad read; stdout={stdout}, stderr={stderr}"
    );
    assert!(
        !stdout.contains("key_rc=0"),
        "the secret key was readable despite the hide-hole; stdout={stdout}, stderr={stderr}"
    );
    assert!(
        stdout.contains("entries=0"),
        "the hide-hole dir was not empty (real contents leaked through); stdout={stdout}, stderr={stderr}"
    );
}

/// **Combo (disjoint):** a readonly-hole and a hide-hole that do NOT overlap, plus
/// a writable positive control. Both holes take effect independently. The control
/// write succeeding also confirms bash ran under the new namespaces (guards against
/// a false pass on a silent unshare/exec failure).
#[tokio::test]
async fn combo_disjoint_readonly_and_hide_both_effective() {
    if skip_if_unsupported() || userns_unavailable() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let ro = unique_dir(&base, "ro"); // → bind+remount-ro
    let hide = unique_dir(&base, "hide"); // → tmpfs overlay
    let writable = unique_dir(&base, "writable"); // positive control
    std::fs::write(ro.join("f"), b"ro").unwrap();
    std::fs::write(hide.join("key"), b"topsecret").unwrap();

    let decision = AccessDecision {
        read: ReadPolicy::Broad,
        writable: vec![writable.clone()],
        readonly_holes: vec![ro.clone()],
        hide_holes: vec![hide.clone()],
    };

    let ro_target = ro.join("f");
    let writable_target = writable.join("f");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "echo x > '{writable_target}'; echo \"writable_rc=$?\"; \
             echo x > '{ro_target}' 2>/dev/null; echo \"ro_rc=$?\"; \
             cat '{hide_key}' 2>/dev/null; echo \"key_rc=$?\"; \
             echo \"entries=$(ls -A '{hide}' 2>/dev/null | wc -l)\"",
            writable_target = writable_target.display(),
            ro_target = ro_target.display(),
            hide_key = hide.join("key").display(),
            hide = hide.display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("writable_rc=0"),
        "positive-control write failed (bash may not have run under the ns); \
         stdout={stdout}, stderr={stderr}"
    );
    assert!(
        !stdout.contains("ro_rc=0"),
        "readonly-hole write unexpectedly succeeded; stdout={stdout}, stderr={stderr}"
    );
    assert!(
        !stdout.contains("key_rc=0"),
        "hide-hole secret was readable; stdout={stdout}, stderr={stderr}"
    );
    assert!(
        stdout.contains("entries=0"),
        "hide-hole dir was not empty; stdout={stdout}, stderr={stderr}"
    );
}

/// **Combo (overlapping — hide is the readonly-hole's ancestor):** pins the
/// mount-stacking winner. `apply()` installs `bind(P/ro)` THEN `tmpfs(P)`; per the
/// documented invariant on `AccessDecision::hide_holes` the two should be prefix-
/// disjoint, but if they AREN'T, the later tmpfs at the ancestor shadows the bind
/// entirely — `P` reads empty and `P/ro` becomes unreachable. This test
/// characterizes that real kernel behavior so a future reorder (e.g. tmpfs before
/// bind) that flips the winner is caught. The writable control proves the ns ran.
#[tokio::test]
async fn combo_hide_ancestor_shadows_readonly_hole() {
    if skip_if_unsupported() || userns_unavailable() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let parent = unique_dir(&base, "parent"); // → hide-hole (tmpfs over the ancestor)
    let ro = unique_dir(&parent, "ro"); // → readonly-hole, NESTED under the hide
    let writable = unique_dir(&base, "writable"); // positive control (disjoint from parent)
    std::fs::write(ro.join("f"), b"real").unwrap();

    let decision = AccessDecision {
        read: ReadPolicy::Broad,
        writable: vec![writable.clone()],
        readonly_holes: vec![ro.clone()],
        hide_holes: vec![parent.clone()],
    };

    let writable_target = writable.join("f");
    let ro_target = ro.join("f");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "echo x > '{writable_target}'; echo \"writable_rc=$?\"; \
             echo \"entries=$(ls -A '{parent}' 2>/dev/null | wc -l)\"; \
             cat '{ro_target}' 2>/dev/null; echo \"ro_rc=$?\"",
            writable_target = writable_target.display(),
            parent = parent.display(),
            ro_target = ro_target.display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("writable_rc=0"),
        "positive-control write failed (bash may not have run under the ns); \
         stdout={stdout}, stderr={stderr}"
    );
    // The tmpfs over the ancestor shadows the nested readonly-hole bind: the parent
    // reads empty and the readonly target is unreachable. If `apply()`'s install
    // order is ever flipped (tmpfs before bind), this assertion fails — the winner
    // has changed and the documented invariant needs revisiting.
    assert!(
        stdout.contains("entries=0"),
        "the hide-hole ancestor did not shadow its subtree (entries non-empty); \
         stdout={stdout}, stderr={stderr}"
    );
    assert!(
        !stdout.contains("ro_rc=0"),
        "the nested readonly-hole target was reachable (ancestor tmpfs did not shadow it); \
         stdout={stdout}, stderr={stderr}"
    );
}

/// **Combo (overlapping — readonly is the hide-hole's ancestor):** the safe
/// overlap direction. `apply()` installs `bind(P)` THEN `tmpfs(P/secret)`; the
/// tmpfs is a child mount of the ro bind, so BOTH are effective: `P` is read-only
/// (writes blocked), `P/secret` is hidden (empty), and the rest of `P` stays
/// readable through the ro bind. Pins this coexistence.
#[tokio::test]
async fn combo_bind_ancestor_and_hide_descendant_both_effective() {
    if skip_if_unsupported() || userns_unavailable() {
        return;
    }
    let Some(base) = non_baseline_parent() else {
        return;
    };
    let parent = unique_dir(&base, "parent"); // → readonly-hole (bind+remount-ro ancestor)
    let secret = unique_dir(&parent, "secret"); // → hide-hole, NESTED under the ro bind
    let writable = unique_dir(&base, "writable"); // positive control (disjoint from parent)
    std::fs::write(parent.join("other"), b"readable").unwrap();
    std::fs::write(secret.join("key"), b"topsecret").unwrap();

    let decision = AccessDecision {
        read: ReadPolicy::Broad,
        writable: vec![writable.clone()],
        readonly_holes: vec![parent.clone()],
        hide_holes: vec![secret.clone()],
    };

    let writable_target = writable.join("f");
    let other = parent.join("other");
    let secret_key = secret.join("key");
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "echo x > '{writable_target}'; echo \"writable_rc=$?\"; \
             echo x > '{other}' 2>/dev/null; echo \"other_write_rc=$?\"; \
             cat '{other}' 2>/dev/null; echo \"other_read_rc=$?\"; \
             cat '{secret_key}' 2>/dev/null; echo \"key_rc=$?\"; \
             echo \"entries=$(ls -A '{secret}' 2>/dev/null | wc -l)\"",
            writable_target = writable_target.display(),
            other = other.display(),
            secret_key = secret_key.display(),
            secret = secret.display(),
        ))
        .kill_on_drop(true);
    apply(&mut cmd, &decision).expect("apply should succeed");
    let output = cmd
        .spawn()
        .expect("spawn should succeed")
        .wait_with_output()
        .await
        .expect("wait should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("writable_rc=0"),
        "positive-control write failed (bash may not have run under the ns); \
         stdout={stdout}, stderr={stderr}"
    );
    // The ro bind on the ancestor blocks writes to its (non-hidden) children…
    assert!(
        !stdout.contains("other_write_rc=0"),
        "write under the readonly-hole ancestor unexpectedly succeeded; \
         stdout={stdout}, stderr={stderr}"
    );
    // …while leaving them readable through the ro bind.
    assert!(
        stdout.contains("other_read_rc=0"),
        "read of a non-hidden file under the ro bind failed; stdout={stdout}, stderr={stderr}"
    );
    // …and the nested hide-hole tmpfs still hides the secret subtree.
    assert!(
        !stdout.contains("key_rc=0"),
        "the nested hide-hole secret was readable; stdout={stdout}, stderr={stderr}"
    );
    assert!(
        stdout.contains("entries=0"),
        "the nested hide-hole dir was not empty; stdout={stdout}, stderr={stderr}"
    );
}
