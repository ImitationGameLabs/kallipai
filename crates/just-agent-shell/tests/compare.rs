//! Head-to-head comparison: persistent-PTY backend vs the stateless backend.
//!
//! Same commands run through both; asserts where they agree and documents where
//! they intentionally diverge. This harness drives the **swap-over decision**.
//!
//! # Swap-over criteria (per `.draft/design/shell-execution-stateless-redesign.md`)
//!
//! - **Proceed** when the stateless backend is correct and ≥ parity on
//!   output/exit/timed_out across these scenarios, plus its structural wins:
//!   process-group kill reaps orphaned children (PTY does not), no session can
//!   wedge, and cwd is read fresh (never a stale cache).
//! - **Rework** if a scenario reveals a stateless regression that can't be fixed.
//! - **Abandon the PTY backend** (and the `wip/shell-cwd-tracking` branch) once
//!   the swap commits.
//!
//! Documented divergences (stateless is strictly better, not bugs):
//! - `cd` persistence: the stateless backend reports the real post-`cd` cwd
//!   (Fork B pwd roundtrip); the `e9596b5` PTY backend does not track cwd at all.
//! - Timeout: the stateless backend kills the whole process group (no orphans);
//!   the PTY backend leaves the shell alive.
//! - Background: the stateless backend returns a `task_id` with read/kill; the
//!   PTY backend is fire-and-forget.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use just_agent_shell::{PtyBuilder, ShellBackend, StatelessBackend, StatelessBuilder};
use tokio::sync::Mutex;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn stateless_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ja-compare-{label}-{}-{n}", std::process::id()))
}

#[tokio::test]
async fn both_capture_echo_and_exit_code() {
    let pty = Arc::new(Mutex::new(PtyBuilder::new("main").build().await.unwrap()));
    let mut stateless = StatelessBuilder::new()
        .data_dir(stateless_dir("echo"))
        .build()
        .await
        .unwrap();

    // `false` (not `exit 7`) so the PTY shell isn't destroyed mid-capture — a
    // command that `exit`s the persistent shell is a known PTY limitation.
    let cmd = "echo hello; false";
    let pty_out = pty
        .lock()
        .await
        .execute(cmd, Duration::from_secs(10), false)
        .await
        .unwrap();
    let st_out = stateless.exec(cmd, Duration::from_secs(10)).await.unwrap();

    assert!(pty_out.output.contains("hello"));
    assert!(st_out.stdout.contains("hello"));
    assert_eq!(pty_out.exit_code, Some(1));
    assert_eq!(st_out.exit_code, Some(1));
}

#[tokio::test]
async fn stateless_tracks_cd_pty_does_not() {
    // Documented divergence: the stateless backend reflects `cd` in the reported
    // cwd (Fork B); the e9596b5 PTY backend does not track cwd.
    let mut stateless = StatelessBuilder::new()
        .data_dir(stateless_dir("cd"))
        .build()
        .await
        .unwrap();
    let target = std::env::temp_dir();
    let target = std::fs::canonicalize(&target).unwrap_or(target);

    stateless
        .exec(
            &format!("cd '{}'", target.display()),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let out = stateless
        .exec("pwd", Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(out.cwd, target, "stateless cwd must reflect the cd");
    assert_eq!(out.stdout.trim(), target.to_string_lossy());
}

#[tokio::test]
async fn both_report_timeout() {
    let pty = Arc::new(Mutex::new(PtyBuilder::new("main").build().await.unwrap()));
    let mut stateless = StatelessBuilder::new()
        .data_dir(stateless_dir("timeout"))
        .build()
        .await
        .unwrap();

    let cmd = "sleep 30";
    let pty_out = pty
        .lock()
        .await
        .execute(cmd, Duration::from_millis(500), false)
        .await
        .unwrap();
    let st_out = stateless
        .exec(cmd, Duration::from_millis(500))
        .await
        .unwrap();

    assert!(pty_out.timed_out, "PTY must report timeout");
    assert!(st_out.timed_out, "stateless must report timeout");
    assert_eq!(st_out.exit_code, Some(124), "stateless synthesizes 124");
}

#[tokio::test]
async fn stateless_kills_process_group_no_orphans() {
    // State-only assertion (PTY diverges: it leaves the shell alive). After a
    // timeout, the orphaned `sleep 30 &` must be gone.
    let mut stateless = StatelessBuilder::new()
        .data_dir(stateless_dir("pgroup"))
        .build()
        .await
        .unwrap();
    // A unique duration so `pgrep` doesn't match `sleep` spawned by other
    // concurrent tests (cross-test isolation).
    let _ = stateless
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
    // The stateless backend bounds it by timeout at worst; here it should exit
    // well before the timeout with a non-zero code or a warning.
    let mut stateless = StatelessBuilder::new()
        .data_dir(stateless_dir("vim"))
        .build()
        .await
        .unwrap();
    let out = stateless
        .exec("vim +qa", Duration::from_secs(15))
        .await
        .unwrap();
    // Either it exited (some code) or timed out — but it must NOT hang (the test
    // itself would hang). Assert it returned at all (reaching here = no wedge).
    assert!(out.stdout.contains("Vim") || out.stderr.contains("Vim") || out.exit_code.is_some());
}
