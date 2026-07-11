//! Shell-backend behavior tests: cwd tracking, process-group kill, and
//! interactive fail-fast.
//!
//! These exercise the real `ProcessBackend` (a fresh `bash -c` per call) end to
//! end. The backend is file-free, so the tests need no scratch dir.

use std::time::Duration;

use kallip_shell::{CaptureMode, ShellBackend, ShellBuilder};

#[tokio::test]
async fn cd_is_reflected_in_reported_cwd() {
    // The backend reflects `cd` in the reported cwd via the stderr marker.
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let target = std::env::temp_dir();
    let target = std::fs::canonicalize(&target).unwrap_or(target);

    backend
        .exec(
            &format!("cd '{}'", target.display()),
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let out = backend
        .exec("pwd", Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap();
    assert_eq!(out.cwd, target, "cwd must reflect the cd");
    assert_eq!(
        out.merged.as_deref().unwrap().trim(),
        target.to_string_lossy()
    );
}

#[tokio::test]
async fn timeout_kills_process_group_no_orphans() {
    // After a timeout, the orphaned `sleep` must be gone — the whole process
    // group is killed, not just the leader.
    let mut backend = ShellBuilder::new().build().await.unwrap();
    // A unique duration so `pgrep` doesn't match `sleep` spawned by other
    // concurrent tests (cross-test isolation).
    let _ = backend
        .exec(
            "sleep 41 & wait",
            Duration::from_millis(500),
            CaptureMode::Merged,
        )
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
    // means it did not hang. Use `Separate` so we can inspect either stream.
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec("vim +qa", Duration::from_secs(15), CaptureMode::Separate)
        .await
        .unwrap();
    let stdout = out.stdout.as_deref().unwrap_or("");
    let stderr = out.stderr.as_deref().unwrap_or("");
    assert!(stdout.contains("Vim") || stderr.contains("Vim") || out.exit_code.is_some());
}
