//! One-shot command execution: a fresh `bash` process per call.
//!
//! Every [`ProcessBackend::exec`] spawns an isolated `bash <wrapper>` (piped
//! stdout/stderr, `stdin` null, its own process group) and captures output until
//! the child exits — then bounds a final pipe drain in case a grandchild holds
//! the write-end open. On timeout the whole process group is killed (SIGTERM →
//! grace → SIGKILL) and exit code 124 is synthesized. The wrapper's `EXIT` trap
//! records `pwd -P` so the sticky cwd is read fresh after every command.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::error::ShellError;
use crate::{builder, capture, cwd, pgroup, supervisor};

/// Exit code synthesized on timeout (matches GNU `timeout(1)`).
const TIMEOUT_EXIT: i32 = 124;
/// Default per-call timeout when the caller omits one.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Color-suppression env vars applied to every spawned `bash` (foreground and
/// background) so tool output is free of escape sequences. Injected via
/// [`Command::env`] by both exec paths, rather than emitted into the wrapper,
/// so the mechanism is uniform and survives any rc the shell sources.
pub(super) const COLOR_VARS: &[(&str, &str)] = &[
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("LS_COLORS", ""),
    ("CLICOLOR", "0"),
];

/// Removes a directory when dropped (best-effort), so a per-call working dir is
/// cleaned up on every exit path — success, early `?` error, or panic.
struct RemoveOnDrop(Option<PathBuf>);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Result of a command execution.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    /// Captured stdout (possibly clipped to a tail).
    pub stdout: String,
    /// Captured stderr (possibly clipped to a tail).
    pub stderr: String,
    /// Process exit code, or `None` on signal death; `Some(124)` on timeout.
    pub exit_code: Option<i32>,
    /// Whether the command exceeded its timeout.
    pub timed_out: bool,
    /// Whether stdout or stderr was clipped (exceeded the byte budget).
    pub truncated: bool,
    /// The working directory after the command (read fresh from `pwd`).
    pub cwd: PathBuf,
}

/// Abstraction for a one-shot command runner.
///
/// There are no sessions, no scrollback, no "current session": every
/// [`ShellBackend::exec`] spawns a fresh process. [`ProcessBackend`] is the
/// concrete implementation; an in-memory mock is available behind the
/// `testutils` feature for downstream tests, so the `bash_exec` tool stays
/// generic over its backend.
#[async_trait]
pub trait ShellBackend: Send + Sync {
    /// Run `command`, returning its output and the post-command cwd.
    async fn exec(&mut self, command: &str, timeout: Duration) -> Result<ShellOutput, ShellError>;
    /// The current (sticky) working directory.
    fn cwd(&self) -> &Path;
    /// Spawn `command` as a background task; returns its id.
    async fn spawn_background(&mut self, command: &str) -> Result<String, ShellError>;
    /// Read accumulated output and status of a background task.
    async fn read_background(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<supervisor::BgReadOutput, ShellError>;
    /// Cancel and reap a background task.
    async fn kill_background(&mut self, id: &str) -> Result<(), ShellError>;
}

/// Concrete backend: one fresh process per call.
pub struct ProcessBackend {
    pub(super) config: builder::ShellBuilder,
    pub(super) cwd: PathBuf,
    pub(super) data_dir: PathBuf,
    pub(super) next_call: u64,
    pub(super) background: supervisor::BackgroundRegistry,
}

#[async_trait]
impl ShellBackend for ProcessBackend {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    async fn exec(
        &mut self,
        command: &str,
        timeout_dur: Duration,
    ) -> Result<ShellOutput, ShellError> {
        // Resolve an existing spawn cwd; fall back if the cached one was deleted.
        let spawn_cwd =
            std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.config.fallback_cwd.clone());

        // Per-call working dir for the wrapper + pwd tmpfile (unique, no collision).
        let call_id = self.next_call;
        self.next_call += 1;
        let call_dir = self.data_dir.join("calls").join(call_id.to_string());
        std::fs::create_dir_all(&call_dir)?;
        let wrapper_path = call_dir.join("cmd.sh");
        let pwd_file = call_dir.join("pwd");

        let wrapper = build_wrapper(command, Some(&pwd_file));
        std::fs::write(&wrapper_path, wrapper)?;

        let mut cmd = Command::new(&self.config.shell);
        cmd.arg(&wrapper_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .current_dir(&spawn_cwd);
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }
        // Color suppression (parity with the background spawn path).
        for (key, value) in COLOR_VARS {
            cmd.env(key, value);
        }
        // Landlock-restrict this bash to the agent's current access decision
        // (Linux + landlock). Compose the decision (lock-manager-backed snapshot
        // + this backend's scratch dir) via `AccessSource`; `apply` is pure
        // mechanism — it moves the prepared landlock/mount-hole state into the
        // `pre_exec` closure, which `cmd` owns until `spawn()` consumes it, so
        // the ruleset fd survives the fork and is read in the child.
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        if let Some(source) = &self.config.access_source {
            crate::landlock::apply(&mut cmd, &source.access_with_scratch(&self.data_dir)?)?;
        }

        let mut child = cmd.spawn()?;
        // Clean up the per-call dir on every exit path (early `?`, panic, success).
        let _call_dir_guard = RemoveOnDrop(Some(call_dir.clone()));

        let max = self.config.max_output_bytes;
        // Shared captures so partial output survives even if a pump is stuck
        // (a grandchild holding the pipe write-end) and has to be aborted.
        let out_cap = Arc::new(Mutex::new(capture::BoundedCapture::new(max)));
        let err_cap = Arc::new(Mutex::new(capture::BoundedCapture::new(max)));
        let out_task = tokio::spawn(pump(child.stdout.take(), out_cap.clone()));
        let err_task = tokio::spawn(pump(child.stderr.take(), err_cap.clone()));

        let (exit_status, timed_out) = run_until_exit_or_timeout(&mut child, timeout_dur).await;

        // Abort any still-blocked pump (a grandchild may hold the write-end) and
        // finalize whatever was buffered — partial output is preserved.
        let out_cap = finish_capture(out_task, out_cap).await;
        let err_cap = finish_capture(err_task, err_cap).await;

        // Read the sticky cwd fresh from the wrapper's EXIT trap.
        let new_cwd = cwd::resolve(&pwd_file, &self.config.fallback_cwd)?;
        self.cwd = new_cwd.clone();

        let exit_code = if timed_out {
            Some(TIMEOUT_EXIT)
        } else {
            exit_status.and_then(|s| s.code())
        };

        Ok(ShellOutput {
            stdout: out_cap.text,
            stderr: err_cap.text,
            exit_code,
            timed_out,
            truncated: out_cap.truncated || err_cap.truncated,
            cwd: new_cwd,
        })
    }

    async fn spawn_background(&mut self, command: &str) -> Result<String, ShellError> {
        self.background.spawn(command)
    }

    async fn read_background(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<supervisor::BgReadOutput, ShellError> {
        self.background.read(id, tail_bytes)
    }

    async fn kill_background(&mut self, id: &str) -> Result<(), ShellError> {
        self.background.kill(id).await
    }
}

/// Wait for `child` to exit naturally, or kill the process group on timeout.
///
/// On timeout, [`pgroup::kill_tree`] kills the whole group and reaps the leader;
/// the caller synthesizes exit code 124, so the real (cached) status is unused.
async fn run_until_exit_or_timeout(
    child: &mut Child,
    timeout_dur: Duration,
) -> (Option<std::process::ExitStatus>, bool) {
    tokio::select! {
        result = child.wait() => (result.ok(), false),
        _ = tokio::time::sleep(timeout_dur) => {
            let _ = pgroup::kill_tree(child).await;
            (None, true)
        }
    }
}

/// Pump a piped stream into a shared bounded capture until EOF or error.
async fn pump(reader: Option<impl AsyncRead + Unpin>, cap: Arc<Mutex<capture::BoundedCapture>>) {
    if let Some(mut r) = reader {
        let mut buf = [0u8; 8 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut c) = cap.lock() {
                        c.push(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    }
}

/// Abort a pump task (it may be blocked on a pipe held open by a grandchild)
/// and finalize whatever it buffered. Partial output survives.
async fn finish_capture(
    handle: tokio::task::JoinHandle<()>,
    cap: Arc<Mutex<capture::BoundedCapture>>,
) -> capture::CaptureResult {
    handle.abort();
    let _ = handle.await; // resolves promptly with Cancelled after abort
    let taken = std::mem::take(&mut *cap.lock().expect("capture lock poisoned"));
    taken.finish()
}

/// Build the per-call (or per-background-task) wrapper script.
///
/// The wrapper does only two things: when `pwd_file` is `Some`, set an `EXIT`
/// trap that records `pwd -P` so the sticky cwd is captured on normal exit,
/// `exit`, and SIGTERM (on SIGKILL it doesn't fire and the caller falls back —
/// honest); then run `command`. Everything else the child needs — env,
/// `config.env`, and color suppression — is injected via [`Command::env`] by
/// the caller, since the spawned bash inherits that env directly. Background
/// tasks pass `None` so they don't mutate the shared sticky cwd.
pub(super) fn build_wrapper(command: &str, pwd_file: Option<&Path>) -> String {
    let mut s = String::with_capacity(128 + command.len());
    // EXIT trap writes the resolved cwd to a tmpfile the backend reads back.
    if let Some(pwd_file) = pwd_file {
        let pwd_q = shell_quote(pwd_file);
        s.push_str(&format!("__ja_pwd() {{ pwd -P >| {pwd_q}; }}\n"));
        s.push_str("trap -- __ja_pwd EXIT\n");
    }
    s.push_str(command);
    s.push('\n');
    s
}

/// Single-quote a path for safe shell interpolation (`'` → `'\''`).
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("'");
    out.push_str(&s.replace('\'', "'\\''"));
    out.push('\'');
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::builder::ShellBuilder;

    #[tokio::test]
    async fn exec_captures_stdout_and_exit_code() {
        let mut backend = ShellBuilder::new()
            .data_dir(test_dir())
            .build()
            .await
            .unwrap();
        let out = backend
            .exec("echo hello; exit 7", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(7));
        assert!(out.stdout.contains("hello"));
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn exec_cd_persists_across_calls() {
        let mut backend = ShellBuilder::new()
            .data_dir(test_dir())
            .build()
            .await
            .unwrap();
        let target = std::env::temp_dir();
        let cd = format!("cd '{}'", target.display());
        backend.exec(&cd, Duration::from_secs(10)).await.unwrap();
        let out = backend.exec("pwd", Duration::from_secs(10)).await.unwrap();
        // cwd is read fresh from pwd after the cd → sticky.
        assert_eq!(out.cwd, std::fs::canonicalize(&target).unwrap());
        assert!(out.stdout.trim() == out.cwd.to_string_lossy());
    }

    #[tokio::test]
    async fn exec_timeout_kills_and_synthesizes_124() {
        let mut backend = ShellBuilder::new()
            .data_dir(test_dir())
            .build()
            .await
            .unwrap();
        let out = backend
            .exec("sleep 30", Duration::from_millis(500))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, Some(124));
    }

    #[tokio::test]
    async fn exec_timeout_reaps_process_group() {
        let mut backend = ShellBuilder::new()
            .data_dir(test_dir())
            .build()
            .await
            .unwrap();
        // `sleep 43 &` orphans a child if only the leader is killed. A unique
        // duration so `pgrep` doesn't match `sleep` spawned by sibling tests
        // running in parallel.
        let _ = backend
            .exec("sleep 43 & wait", Duration::from_millis(500))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pgrep = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("sleep 43")
            .output()
            .unwrap();
        assert!(
            pgrep.stdout.is_empty(),
            "orphaned `sleep 43` survived: {:?}",
            String::from_utf8_lossy(&pgrep.stdout)
        );
    }

    /// A non-zero exit still fires the EXIT trap, so the sticky cwd roundtrip
    /// reports the post-command directory (the existing `exit 7` test never
    /// asserted cwd).
    #[tokio::test]
    async fn exit_n_traps_and_reports_cwd() {
        let dir = test_dir();
        let mut backend = ShellBuilder::new()
            .data_dir(dir.clone())
            .build()
            .await
            .unwrap();
        let dir_a = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir_b = std::fs::canonicalize(&dir).unwrap();
        backend
            .exec(
                &format!("cd '{}'", dir_a.display()),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' ; exit 42", dir_b.display()),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(42));
        assert_eq!(out.cwd, dir_b);
        // Sticky cwd persists to the next call.
        let out = backend.exec("pwd", Duration::from_secs(10)).await.unwrap();
        assert_eq!(out.cwd, dir_b);
    }

    /// If a command removes its own cwd, the trap's `pwd` write targets a gone
    /// directory and `cwd::resolve`'s canonicalize guard falls back rather than
    /// reporting a stale path.
    #[tokio::test]
    async fn deleted_cwd_falls_back() {
        let dir = test_dir();
        let mut backend = ShellBuilder::new()
            .data_dir(dir.clone())
            .build()
            .await
            .unwrap();
        let doomed = dir.join("doomed");
        std::fs::create_dir_all(&doomed).unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' && rmdir '{}'", doomed.display(), doomed.display()),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(
            out.cwd.exists(),
            "cwd should fall back to an existing dir, not the deleted one; got {}",
            out.cwd.display()
        );
    }

    /// Color-suppression env vars reach the spawned bash via `Command::env`
    /// (the wrapper no longer emits them): all four `COLOR_VARS` entries are
    /// applied — `TERM`/`NO_COLOR`/`CLICOLOR` set, `LS_COLORS` emptied.
    #[tokio::test]
    async fn color_vars_suppress_in_foreground() {
        let mut backend = ShellBuilder::new()
            .data_dir(test_dir())
            .build()
            .await
            .unwrap();
        // Covers all four COLOR_VARS entries: TERM/NO_COLOR/CLICOLOR set,
        // LS_COLORS emptied.
        let out = backend
            .exec(
                "echo \"$TERM/$NO_COLOR/$CLICOLOR\"; test -z \"$LS_COLORS\" && echo empty",
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout.trim(), "dumb/1/0\nempty");
    }

    /// The snapshot subsystem is gone: building a backend must NOT create
    /// `env.sh` in the data dir.
    #[tokio::test]
    async fn build_does_not_write_env_sh() {
        let dir = test_dir();
        let _backend = ShellBuilder::new()
            .data_dir(dir.clone())
            .build()
            .await
            .unwrap();
        assert!(!dir.join("env.sh").exists(), "env.sh should not be created");
    }

    /// A var set via the builder reaches the command (process inheritance +
    /// `Command::env` replace the removed snapshot).
    #[tokio::test]
    async fn builder_env_reaches_exec() {
        let mut backend = ShellBuilder::new()
            .data_dir(test_dir())
            .env("JA_INHERIT_PROBE", "ok")
            .build()
            .await
            .unwrap();
        let out = backend
            .exec(
                "echo \"${JA_INHERIT_PROBE:?unset}\"",
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout.trim(), "ok");
    }

    fn test_dir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("ja-shell-test-{}-{n}", std::process::id()))
    }
}
