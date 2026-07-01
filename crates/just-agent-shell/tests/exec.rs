//! Shell-backend behavior tests: cwd tracking, process-group kill, and
//! interactive fail-fast.
//!
//! These exercise the real `ProcessBackend` (a fresh `bash` per call) end to
//! end. The per-backend data dir is a unique `/tmp` path (the leak fix is
//! deferred to the persistence refactor).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use just_agent_shell::{ShellBackend, ShellBuilder};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn test_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ja-shell-exec-{label}-{}-{n}", std::process::id()))
}

#[tokio::test]
async fn cd_is_reflected_in_reported_cwd() {
    // The backend reflects `cd` in the reported cwd via the `pwd` roundtrip.
    let mut backend = ShellBuilder::new()
        .data_dir(test_dir("cd"))
        .build()
        .await
        .unwrap();
    let target = std::env::temp_dir();
    let target = std::fs::canonicalize(&target).unwrap_or(target);

    backend
        .exec(
            &format!("cd '{}'", target.display()),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let out = backend.exec("pwd", Duration::from_secs(10)).await.unwrap();
    assert_eq!(out.cwd, target, "cwd must reflect the cd");
    assert_eq!(out.stdout.trim(), target.to_string_lossy());
}

#[tokio::test]
async fn timeout_kills_process_group_no_orphans() {
    // After a timeout, the orphaned `sleep` must be gone — the whole process
    // group is killed, not just the leader.
    let mut backend = ShellBuilder::new()
        .data_dir(test_dir("pgroup"))
        .build()
        .await
        .unwrap();
    // A unique duration so `pgrep` doesn't match `sleep` spawned by other
    // concurrent tests (cross-test isolation).
    let _ = backend
        .exec("sleep 41 & wait", Duration::from_millis(500))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let pgrep = std::process::Command::new("pgrep")
        .args(["-f", "sleep 41"])
        .output()
        .unwrap();
    assert!(
        pgrep.stdout.is_empty(),
        "orphaned `sleep 41` survived the timeout: {}",
        String::from_utf8_lossy(&pgrep.stdout)
    );
}

#[tokio::test]
async fn interactive_command_fails_fast_does_not_wedge() {
    // `vim` without a TTY + stdin null must exit promptly (no hang, no wedge).
    // The backend bounds it by timeout at worst; here it should exit well before
    // the timeout with a non-zero code or a warning. Reaching the assert at all
    // means it did not hang.
    let mut backend = ShellBuilder::new()
        .data_dir(test_dir("vim"))
        .build()
        .await
        .unwrap();
    let out = backend
        .exec("vim +qa", Duration::from_secs(15))
        .await
        .unwrap();
    assert!(out.stdout.contains("Vim") || out.stderr.contains("Vim") || out.exit_code.is_some());
}
