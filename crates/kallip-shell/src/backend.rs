//! One-shot command execution: a fresh `bash` process per call.
//!
//! Every [`ProcessBackend::exec`] spawns an isolated `bash -c <script>` (piped
//! stdout/stderr, `stdin` null, its own process group) and writes **no files**:
//! the script rides argv, and the post-command cwd rides a marker the script's
//! `EXIT` trap prints to stderr. Output is captured until the child exits, then
//! a final pipe drain is bounded in case a grandchild holds the write-end open.
//! On timeout the whole process group is killed (SIGTERM -> grace -> SIGKILL)
//! and exit code 124 is synthesized. If the future is dropped before completion
//! (the runtime cancels the tool call), a `GroupKillGuard` force-kills the
//! whole group so grandchildren do not survive the leader. The trap fires on
//! normal exit, `exit`, and SIGTERM, so the sticky cwd is read fresh after
//! every command. A SIGKILL before the trap, a wedged pipe, or `exec
//! 2>/dev/null` loses the marker, in which case the caller falls back (never a
//! stale path); `exec 2>&1` moves the marker onto stdout, where it is still
//! recovered and stripped. A grandchild that the command intentionally
//! backgrounded and detached on the *normal* exit path (e.g. `sleep 99 &
//! disown; exit`) is not killed -- that is an intentional non-goal (use
//! `spawn_background` for durable background work); only the cancel path
//! force-kills the group.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::error::ShellError;
use crate::{builder, capture, cwd, pgroup, supervisor};

/// Exit code synthesized on timeout (matches GNU `timeout(1)`).
const TIMEOUT_EXIT: i32 = 124;
/// Default per-call timeout when the caller omits one.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Ceiling on the inline `bash -c` script size, in bytes. The kernel would
/// allow up to `MAX_ARG_STRLEN` (128 KiB on a 4 KiB-page kernel), but a script
/// that large has no business riding an argv string — large content should be
/// staged in a file and run as `bash <file>`. So this is set deliberately
/// lower as a "use a file" guardrail. The trap prefix adds only a few dozen
/// bytes, well within the margin.
const MAX_SCRIPT_BYTES: usize = 8 * 1024;

/// Color-suppression env vars applied to every spawned `bash` (foreground and
/// background) so tool output is free of escape sequences. Injected via
/// [`Command::env`] by both exec paths, rather than baked into the script, so
/// the mechanism is uniform and survives any rc the shell sources.
pub(super) const COLOR_VARS: &[(&str, &str)] = &[
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("LS_COLORS", ""),
    ("CLICOLOR", "0"),
];

/// How [`ShellBackend::exec`] captures a command's output.
///
/// `Merged` (the default) interleaves stdout and stderr into a single stream,
/// like `2>&1` — the natural "run a command" experience, where any ordering
/// between the two is the program's own responsibility (it flushes to enforce
/// it). The other variants trade that for stream separation or selection, e.g.
/// to parse clean stdout without diagnostic noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// stdout and stderr merged into one stream (program-determined ordering).
    #[default]
    Merged,
    /// stdout and stderr captured and returned as separate fields.
    Separate,
    /// Only stdout. Stderr is still captured internally to recover the cwd
    /// marker, then discarded.
    Stdout,
    /// Only stderr. Stdout is still captured internally to recover the cwd
    /// marker (when the command redirected fd 2 onto it), then discarded.
    Stderr,
}

/// Result of a command execution. Exactly the [`CaptureMode`]'s output fields
/// are `Some` (`Merged` -> `merged`; `Separate` -> `stdout` + `stderr`; `Stdout` ->
/// `stdout`; `Stderr` -> `stderr`); the rest are `None`, so the tool layer can
/// omitempty-tag them and the caller sees only what it asked for. Any
/// cwd-recovery marker is stripped from whichever stream(s) carry it.
#[derive(Debug, Clone, Default)]
pub struct ShellOutput {
    /// Merged stdout+stderr (possibly clipped to a tail). `Some` only under
    /// [`CaptureMode::Merged`]; the marker is stripped before return.
    pub merged: Option<String>,
    /// Captured stdout (possibly clipped to a tail). Any cwd-recovery marker
    /// that landed here (e.g. after `exec 2>&1`) is stripped before return.
    pub stdout: Option<String>,
    /// Captured stderr (possibly clipped to a tail). The cwd-recovery marker is
    /// stripped before this is returned.
    pub stderr: Option<String>,
    /// Process exit code, or `None` on signal death; `Some(124)` on timeout.
    pub exit_code: Option<i32>,
    /// Whether the command exceeded its timeout.
    pub timed_out: bool,
    /// Whether a returned stream was clipped (exceeded the byte budget). Only
    /// the stream(s) the mode returns are considered; clipping a discarded
    /// stream is not reported.
    pub truncated: bool,
    /// The working directory after the command (read fresh from the cwd marker).
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
    /// Run `command`, capturing output per `capture`, and return the
    /// post-command cwd.
    async fn exec(
        &mut self,
        command: &str,
        timeout: Duration,
        capture: CaptureMode,
    ) -> Result<ShellOutput, ShellError>;
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
        capture: CaptureMode,
    ) -> Result<ShellOutput, ShellError> {
        // Resolve an existing spawn cwd; fall back if the cached one was deleted.
        let spawn_cwd =
            std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.config.fallback_cwd.clone());

        // Build the `-c` script (EXIT-trap marker + command) and reject an
        // oversized script up front with an actionable error.
        let marker = CwdMarker::new();
        let script = build_exec_script(command, &marker, capture);
        if script.len() > MAX_SCRIPT_BYTES {
            return Err(ShellError::command_too_large(MAX_SCRIPT_BYTES));
        }

        let mut cmd = Command::new(&self.config.shell);
        cmd.arg("-c")
            .arg(&script)
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
        // Merged mode is realized at the script level (`exec 2>&1` prepended by
        // [`build_exec_script`]), not via an fd dup2 in `pre_exec`: the shell
        // itself points fd 2 at the stdout pipe, so a single stdout pump captures
        // the interleaved stream with program-determined ordering. This keeps the
        // Command setup uniform (no platform-specific dup2) and reuses the
        // already-proven marker path -- the trap's `>&2` lands on the merged
        // stdout stream, where it is recovered and stripped. After the merge no
        // process writes the stderr pipe, so its read-end is not pumped below.

        // Landlock-restrict this bash to the agent's current access decision
        // (Linux + landlock). The foreground path writes nothing on disk, so it
        // needs no scratch beyond `baseline_writable` (`/tmp`, `/dev/null`, ...)
        // already folded in by `apply`; the snapshot's writable set carries the
        // agent's own workspace write-locks. `apply` is pure mechanism: it moves
        // the prepared landlock/mount-hole state into the `pre_exec` closure,
        // which `cmd` owns until `spawn()` consumes it, so the ruleset fd
        // survives the fork and is read in the child.
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        if let Some(source) = &self.config.access_source {
            crate::landlock::apply(&mut cmd, &source.access()?)?;
        }

        let mut child = cmd.spawn()?;
        // If this future is dropped while the child is still running (the
        // runtime cancels the tool call), force-kill the whole process group so
        // grandchildren do not survive the leader. `kill_on_drop(true)` on the
        // `Child` (above) is retained as defense-in-depth but only signals the
        // leader; this guard reaches the group, mirroring the background
        // supervisor's registry `Drop`. Disarmed on the success path before
        // returning, so a normal completion does not fire a redundant kill. On
        // cancel, the detached pump tasks below self-terminate once the group
        // kill closes the pipe (they see EOF) -- no separate cleanup.
        let mut kill_guard = GroupKillGuard(child.id());

        let max = self.config.max_output_bytes;
        // Shared captures so partial output survives even if a pump is stuck
        // (a grandchild holding the pipe write-end) and has to be aborted.
        let out_cap = Arc::new(Mutex::new(capture::BoundedCapture::new(max)));
        let err_cap = Arc::new(Mutex::new(capture::BoundedCapture::new(max)));
        let out_task = tokio::spawn(pump(child.stdout.take(), out_cap.clone()));
        // In Merged mode the script's `exec 2>&1` points fd 2 at the stdout
        // pipe, so the stderr pipe carries nothing: skip its pump and drop the
        // read-end (immediate EOF, no hang). Stdout/Stderr modes still pipe both
        // because the cwd marker rides fd 2 and must be recovered even when only
        // one stream is returned.
        let err_task = if capture == CaptureMode::Merged {
            drop(child.stderr.take());
            None
        } else {
            Some(tokio::spawn(pump(child.stderr.take(), err_cap.clone())))
        };

        let (exit_status, timed_out) = run_until_exit_or_timeout(&mut child, timeout_dur).await;

        // Abort any still-blocked pump (a grandchild may hold the write-end) and
        // finalize whatever was buffered — partial output is preserved.
        let out_cap = finish_capture(out_task, out_cap).await;
        let err_cap = match err_task {
            Some(task) => finish_capture(task, err_cap).await,
            // No stderr pump (Merged): empty, untruncated capture placeholder.
            None => capture::CaptureResult::default(),
        };

        // Recover the post-command cwd from the EXIT-trap marker, and strip
        // marker lines from both streams so none reach the LLM. The trap writes
        // to stderr; a command that persistently redirects fd 2 (e.g. `exec
        // 2>&1`) can move the marker onto stdout, so scan stdout as a fallback.
        // `exec 2>/dev/null` (or a SIGKILL before the trap, or a wedged pipe)
        // loses the marker entirely -> fall back. The fallback is always an
        // existing dir, never a stale path.
        let pwd = marker
            .extract_pwd(&err_cap.text)
            .or_else(|| marker.extract_pwd(&out_cap.text));
        let new_cwd = match pwd {
            Some(p) => cwd::resolve_str(&p, &self.config.fallback_cwd),
            None => self.config.fallback_cwd.clone(),
        };
        self.cwd = new_cwd.clone();

        let exit_code = if timed_out {
            Some(TIMEOUT_EXIT)
        } else {
            exit_status.and_then(|s| s.code())
        };

        // The child has settled (exited normally or kill_tree'd on timeout) and
        // the pumps are drained -- disarm so the guard does not fire a redundant
        // group kill when the future otherwise finishes dropping.
        kill_guard.disarm();

        // Strip the marker from whichever stream(s) carry it (no-op where it is
        // absent), then surface only the field(s) the capture mode returns.
        // `truncated` considers just those returned streams — clipping a stream
        // the mode discards is not reported.
        let stripped_out = marker.strip(&out_cap.text);
        let stripped_err = marker.strip(&err_cap.text);
        let (merged, stdout, stderr, truncated) = match capture {
            CaptureMode::Merged => (Some(stripped_out), None, None, out_cap.truncated),
            CaptureMode::Separate => (
                None,
                Some(stripped_out),
                Some(stripped_err),
                out_cap.truncated || err_cap.truncated,
            ),
            CaptureMode::Stdout => (None, Some(stripped_out), None, out_cap.truncated),
            CaptureMode::Stderr => (None, None, Some(stripped_err), err_cap.truncated),
        };

        Ok(ShellOutput {
            merged,
            stdout,
            stderr,
            exit_code,
            timed_out,
            truncated,
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

/// Force-SIGKILL the child's process group on drop, unless disarmed.
///
/// Covers the cancellation path of `exec`: if the future is dropped while the
/// child is still running (the runtime cancels the tool call), the whole group
/// is killed so grandchildren do not survive the leader. `kill_on_drop` on the
/// `Child` only signals the leader; this guard reaches the group via
/// [`pgroup::force_kill_group`], mirroring `BackgroundRegistry::drop`. The pid
/// is the PGID, since `process_group(0)` makes the child the group leader.
/// Disarmed on the success path once the child has settled, so a normal return
/// does not fire a redundant kill.
struct GroupKillGuard(Option<u32>);

impl GroupKillGuard {
    /// Mark the child as settled; its drop becomes a no-op.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            pgroup::force_kill_group(pid as i32);
        }
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

/// Deadline to drain a pump after the child has exited. On the normal path the
/// pipe's write-end closes with the child, so the pump's next read returns EOF
/// within microseconds; this bound only ever binds when a grandchild the command
/// backgrounded still holds the write-end open.
const PUMP_DRAIN_DEADLINE: Duration = Duration::from_secs(1);

/// Finalize a pump, preserving every buffered byte. Let it drain naturally
/// first (after the child exits the pump completes on EOF), so an abort can never
/// drop bytes the kernel has buffered but the pump has not yet read -- a race the
/// unconditional abort could occasionally hit when a pump was mid-read. Only if
/// the pump is still blocked past [`PUMP_DRAIN_DEADLINE`] (a grandchild holding
/// the write-end) is it aborted; partial output survives either way.
async fn finish_capture(
    mut handle: tokio::task::JoinHandle<()>,
    cap: Arc<Mutex<capture::BoundedCapture>>,
) -> capture::CaptureResult {
    if tokio::time::timeout(PUMP_DRAIN_DEADLINE, &mut handle)
        .await
        .is_err()
    {
        // Grandchild-held pipe: cancel the stuck pump, keep what it buffered.
        handle.abort();
        let _ = handle.await; // resolves promptly with Cancelled after abort
    }
    let taken = std::mem::take(&mut *cap.lock().expect("capture lock poisoned"));
    taken.finish()
}

// -- foreground cwd-recovery marker ------------------------------------------

/// The cwd marker's fixed affixes; the `<nonce>` slot is filled per call. The
/// emitted line is `\n__JA_CWD_<nonce>__:<pwd -P>\n` on stderr: the leading
/// newline separates the marker from command output that lacks a trailing one,
/// and the payload sits between the `:` and the terminating newline. The affixes
/// appear both in the trap's `printf` format and in the search needle, so they
/// are defined once here.
const MARKER_HEAD: &str = "__JA_CWD_";
const MARKER_TAIL: &str = "__:";

/// Per-call cwd-recovery marker emitted by the foreground script's `EXIT` trap.
///
/// The trap prints, to stderr, `\n__JA_CWD_<nonce>__:<pwd -P>\n`. After the
/// capture pumps finish, [`CwdMarker::extract`] finds the marker, pulls out the
/// pwd payload, and strips every marker line from the returned stderr. stdout
/// is never touched. The nonce is a random 128-bit value so a command replaying
/// an old log cannot shadow the real emission; `extract` takes the *last* match
/// regardless, and the payload still passes `canonicalize` (a forged
/// non-existent path falls back). Not a security boundary.
struct CwdMarker {
    nonce: String,
}

impl CwdMarker {
    /// Fresh unguessable marker (uuid v4, simple hex form).
    fn new() -> Self {
        Self {
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    /// The needle used to locate the marker: a leading newline (so it matches
    /// the trap's emitted separator) + the fixed head + nonce + tail, ending at
    /// the `:` that precedes the pwd payload.
    fn needle(&self) -> String {
        format!("\n{MARKER_HEAD}{}{MARKER_TAIL}", self.nonce)
    }

    /// The EXIT-trap snippet that emits the marker to stderr. Embedded verbatim
    /// into the `-c` script; the nonce is a literal (never an exported var), so
    /// the command cannot read it via the environment. `printf` emits the bytes
    /// exactly; `>&2` targets the stderr pipe (independent of stdout, so a
    /// grandchild wedging stdout can't block the marker); the leading `\n`
    /// separates the marker from unterminated command output.
    fn trap_script(&self) -> String {
        format!(
            "__ja_pwd() {{ printf '\\n{MARKER_HEAD}{n}{MARKER_TAIL}%s\\n' \"$(pwd -P)\" >&2; }}\ntrap -- __ja_pwd EXIT\n",
            n = self.nonce
        )
    }

    /// Extract the cwd payload (the last marker's `pwd -P`) from a stream, or
    /// `None` when no marker is present. Scans whichever stream the caller
    /// passes — normally stderr, but stdout as a fallback when the command
    /// redirected fd 2.
    fn extract_pwd(&self, text: &str) -> Option<String> {
        let needle = self.needle();
        let last = text.rfind(&needle)?;
        // pwd payload = bytes between the `:` (end of needle) and the next `\n`.
        let pwd_start = last + needle.len();
        let pwd_end = text[pwd_start..]
            .find('\n')
            .map(|i| pwd_start + i)
            .unwrap_or(text.len());
        Some(text[pwd_start..pwd_end].to_owned())
    }

    /// Strip every marker line from a stream, returning the cleaned text. Used
    /// on both stdout and stderr so no marker bytes reach the LLM regardless of
    /// which channel the command left fd 2 pointing at.
    fn strip(&self, text: &str) -> String {
        strip_marker_lines(text, &self.needle())
    }

    /// Extract the cwd payload from a single stream and strip every marker
    /// line from it: `(cleaned, Some(pwd))` when present, `(unchanged, None)`
    /// otherwise. A convenience for tests covering one channel at a time.
    #[cfg(test)]
    fn extract(&self, stderr: &str) -> (String, Option<String>) {
        (self.strip(stderr), self.extract_pwd(stderr))
    }
}

/// Remove every marker line from `text`. The needle begins with the marker's
/// leading `\n`; the marker line spans from that `\n` (at the match index)
/// through the terminating `\n` after the pwd payload. Everything before the
/// leading `\n` is kept; the needle, payload, and terminator are dropped. The
/// needle is ASCII, so byte indices from `str::find` land on char boundaries.
fn strip_marker_lines(text: &str, needle: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(needle) {
        // Keep everything before the marker's leading `\n` (at `idx`).
        out.push_str(&rest[..idx]);
        // Skip from `idx` (the leading `\n`) past the pwd payload's terminating
        // `\n`. The payload sits right after the needle.
        let after = &rest[idx + needle.len()..];
        match after.find('\n') {
            Some(nl) => rest = &after[nl + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Build the foreground `-c` script: install the EXIT-trap marker, then run the
/// command. The whole string is passed as `bash -c`'s single argv element. Under
/// [`CaptureMode::Merged`] an `exec 2>&1` is inserted after the trap so the shell
/// itself merges stderr onto the stdout pipe (program-determined ordering); the
/// trap's `>&2` then lands on that merged stream, where it is recovered/stripped.
fn build_exec_script(command: &str, marker: &CwdMarker, capture: CaptureMode) -> String {
    let mut s = String::with_capacity(256 + command.len());
    s.push_str(&marker.trap_script());
    if capture == CaptureMode::Merged {
        s.push_str("exec 2>&1\n");
    }
    s.push_str(command);
    s.push('\n');
    s
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::builder::ShellBuilder;

    #[tokio::test]
    async fn exec_captures_stdout_and_exit_code() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                "echo hello; exit 7",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(7));
        assert!(out.merged.as_deref().unwrap().contains("hello"));
        assert!(out.stdout.is_none() && out.stderr.is_none());
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn exec_cd_persists_across_calls() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let target = std::env::temp_dir();
        let cd = format!("cd '{}'", target.display());
        backend
            .exec(&cd, Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap();
        let out = backend
            .exec("pwd", Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap();
        // cwd is read fresh from the stderr marker after the cd -> sticky.
        assert_eq!(out.cwd, std::fs::canonicalize(&target).unwrap());
        assert!(out.merged.as_deref().unwrap().trim() == out.cwd.to_string_lossy());
        // The marker rides the merged stream (fd 2 -> stdout via `exec 2>&1`)
        // and is stripped before return: no marker bytes leak.
        assert!(!out.merged.as_deref().unwrap().contains("__JA_CWD_"));
    }

    #[tokio::test]
    async fn exec_timeout_kills_and_synthesizes_124() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec("sleep 30", Duration::from_millis(500), CaptureMode::Merged)
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, Some(124));
    }

    /// On a SIGTERM-honoring timeout the EXIT trap still fires, so the cwd from
    /// a preceding `cd` is reported (not the fallback). Only commands that
    /// ignore SIGTERM and get SIGKILLed lose the cwd.
    #[tokio::test]
    async fn exec_timeout_with_sigterm_trap_reports_cwd() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let target = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        // `trap '' TERM` would ignore; default bash honors SIGTERM by running
        // the EXIT trap during shutdown. `cd` then `sleep` past the timeout.
        let cmd = format!("cd '{}'; sleep 30", target.display());
        let out = backend
            .exec(&cmd, Duration::from_millis(500), CaptureMode::Merged)
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(
            out.cwd, target,
            "trap should fire on SIGTERM and report the cd target"
        );
    }

    #[tokio::test]
    async fn exec_timeout_reaps_process_group() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        // `sleep 43 &` orphans a child if only the leader is killed. A unique
        // duration so `pgrep` doesn't match `sleep` spawned by sibling tests
        // running in parallel.
        let _ = backend
            .exec(
                "sleep 43 & wait",
                Duration::from_millis(500),
                CaptureMode::Merged,
            )
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

    /// Dropping the `exec` future before its own timeout (the runtime's cancel
    /// / tool-timeout path) must kill the whole process group, not just the
    /// leader. The backend timeout (30s) outlasts the outer drop (500ms), so
    /// the cancel path is exercised, not the backend's `kill_tree` timeout.
    #[tokio::test]
    async fn exec_cancel_kills_process_group_no_orphans() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let outer = tokio::time::timeout(
            Duration::from_millis(500),
            backend.exec(
                "sleep 44 & wait",
                Duration::from_secs(30),
                CaptureMode::Merged,
            ),
        )
        .await;
        // The outer timeout must fire (cancel path), not the backend's 30s one.
        assert!(outer.is_err(), "outer timeout should have fired, not exec");
        // The orphaned group is reaped asynchronously after the SIGKILL, so poll
        // for it to be gone rather than asserting instantaneously (follows the
        // polling shape of `pgroup::tests::kill_tree_reaps_orphaned_child`).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pgrep = std::process::Command::new("pgrep")
                .arg("-f")
                .arg("sleep 44")
                .output()
                .unwrap();
            if pgrep.stdout.is_empty() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "orphaned `sleep 44` survived cancel: {}",
                String::from_utf8_lossy(&pgrep.stdout)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A non-zero exit still fires the EXIT trap, so the sticky cwd roundtrip
    /// reports the post-command directory.
    #[tokio::test]
    async fn exit_n_traps_and_reports_cwd() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let dir_a = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let tmp_b = tempfile::TempDir::new().unwrap();
        let dir_b = std::fs::canonicalize(tmp_b.path()).unwrap();
        backend
            .exec(
                &format!("cd '{}'", dir_a.display()),
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' ; exit 42", dir_b.display()),
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(42));
        assert_eq!(out.cwd, dir_b);
        // Sticky cwd persists to the next call.
        let out = backend
            .exec("pwd", Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap();
        assert_eq!(out.cwd, dir_b);
    }

    /// If a command removes its own cwd, the marker's payload targets a gone
    /// directory and `cwd::resolve_str`'s canonicalize guard falls back rather
    /// than reporting a stale path.
    #[tokio::test]
    async fn deleted_cwd_falls_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        let doomed = tmp.path().join("doomed");
        std::fs::create_dir_all(&doomed).unwrap();
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' && rmdir '{}'", doomed.display(), doomed.display()),
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert!(
            out.cwd.exists(),
            "cwd should fall back to an existing dir, not the deleted one; got {}",
            out.cwd.display()
        );
    }

    /// Under `CaptureMode::Merged` the script prepends `exec 2>&1`, so fd 2
    /// points at the stdout pipe and the cwd marker (written to fd 2) lands on
    /// the merged stream. The marker is still recovered there (sticky cwd
    /// updates) and stripped from the returned `merged` so no marker bytes reach
    /// the LLM. A command that *also* does its own `exec 2>&1` is a no-op on top.
    #[tokio::test]
    async fn exec_with_merged_stderr_recovers_cwd_and_strips_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = std::fs::canonicalize(tmp.path()).unwrap();
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' && exec 2>&1 && echo merged", target.display()),
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(
            out.cwd, target,
            "marker on the merged stream must still recover the cwd"
        );
        // Only `merged` is populated; stdout/stderr are None under Merged.
        let merged = out.merged.as_deref().unwrap();
        assert!(out.stdout.is_none() && out.stderr.is_none());
        // The command's own output survives; the marker does not leak.
        assert!(merged.contains("merged"), "merged: {merged}");
        assert!(!merged.contains("__JA_CWD_"), "marker leaked into merged");
    }

    /// Color-suppression env vars reach the spawned bash via `Command::env`
    /// (the script no longer emits them): all four `COLOR_VARS` entries are
    /// applied — `TERM`/`NO_COLOR`/`CLICOLOR` set, `LS_COLORS` emptied.
    #[tokio::test]
    async fn color_vars_suppress_in_foreground() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                "echo \"$TERM/$NO_COLOR/$CLICOLOR\"; test -z \"$LS_COLORS\" && echo empty",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.merged.as_deref().unwrap().trim(), "dumb/1/0\nempty");
    }

    /// Foreground `exec` leaves no scratch behind: the script rides argv and
    /// the cwd rides a stderr marker, so no per-call scratch dir is created in
    /// the spawn cwd. The headline invariant of this refactor. (It does not
    /// snapshot `/tmp`; the structural guarantee that the shell crate has no
    /// `data_dir` concept makes that unnecessary.)
    #[tokio::test]
    async fn exec_leaves_no_scratch_in_cwd() {
        let probe = std::env::temp_dir().join(format!(
            "ja-shell-probe-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&probe).unwrap();
        let mut backend = ShellBuilder::new()
            .initial_cwd(probe.clone())
            .build()
            .await
            .unwrap();
        let _ = backend
            .exec("echo hi", Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap();
        // Only the probe dir the test created should exist; no scratch artifact.
        let mut entries = std::fs::read_dir(&probe).unwrap();
        assert!(
            entries.next().is_none(),
            "foreground exec left files behind in {}",
            probe.display()
        );
        let _ = std::fs::remove_dir_all(&probe);
    }

    /// A command whose `bash -c` script exceeds `MAX_SCRIPT_BYTES` is rejected
    /// up front with an actionable error, before any spawn is attempted (so no
    /// partial side effects and no process is started).
    #[tokio::test]
    async fn exec_rejects_oversized_command() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        // 9000 bytes of payload -> script well over the 8 KiB cap.
        let oversized = format!("printf '{}'", "x".repeat(9000));
        let err = backend
            .exec(&oversized, Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ShellError::CommandTooLarge { limit } if limit == MAX_SCRIPT_BYTES
        ));
    }

    /// A var set via the builder reaches the command (process inheritance +
    /// `Command::env` replace the removed snapshot).
    #[tokio::test]
    async fn builder_env_reaches_exec() {
        let mut backend = ShellBuilder::new()
            .env("JA_INHERIT_PROBE", "ok")
            .build()
            .await
            .unwrap();
        let out = backend
            .exec(
                "echo \"${JA_INHERIT_PROBE:?unset}\"",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.merged.as_deref().unwrap().trim(), "ok");
    }

    // -- CaptureMode coverage -------------------------------------------------

    /// `Merged` (the default) interleaves stdout and stderr into one stream via
    /// the `exec 2>&1` prepended to the script: both `out` and `err` reach the
    /// single `merged` field; `stdout`/`stderr` stay `None`.
    #[tokio::test]
    async fn exec_merged_interleaves_streams() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                "echo out; echo err >&2",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        let merged = out.merged.as_deref().unwrap();
        assert!(merged.contains("out"), "merged: {merged}");
        assert!(merged.contains("err"), "merged: {merged}");
        assert!(out.stdout.is_none() && out.stderr.is_none());
        assert!(!out.truncated);
    }

    /// `Separate` keeps the two streams apart: stdout has `out` (not `err`),
    /// stderr has `err` (not `out`), `merged` is `None`.
    #[tokio::test]
    async fn exec_separate_keeps_streams_apart() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                "echo out; echo err >&2",
                Duration::from_secs(10),
                CaptureMode::Separate,
            )
            .await
            .unwrap();
        let stdout = out.stdout.as_deref().unwrap();
        let stderr = out.stderr.as_deref().unwrap();
        assert!(stdout.contains("out") && !stdout.contains("err"));
        assert!(stderr.contains("err") && !stderr.contains("out"));
        assert!(out.merged.is_none());
    }

    /// `Stdout` returns only stdout but still recovers the cwd, because the
    /// marker rides fd 2 and stderr is captured internally for the marker even
    /// though it is not returned. The returned `stderr` is `None`.
    #[tokio::test]
    async fn exec_stdout_mode_recovers_cwd_from_stderr() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = std::fs::canonicalize(tmp.path()).unwrap();
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' && echo hi", target.display()),
                Duration::from_secs(10),
                CaptureMode::Stdout,
            )
            .await
            .unwrap();
        assert_eq!(out.stdout.as_deref().unwrap().trim(), "hi");
        assert!(out.stderr.is_none() && out.merged.is_none());
        assert_eq!(
            out.cwd, target,
            "Stdout mode must still recover cwd from the internally-captured stderr marker"
        );
    }

    /// `Stderr` mode pins the documented behavior when the command itself
    /// redirects fd 2 onto stdout (`exec 2>&1`): the redirect moves both the
    /// command output and the EXIT-trap marker onto stdout, so nothing ever
    /// writes the stderr pipe and the returned `stderr` is empty. The cwd is
    /// still recovered -- from the internally-captured stdout, via the extract
    /// fallback that scans it for the marker.
    #[tokio::test]
    async fn exec_stderr_mode_pins_down_command_merge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = std::fs::canonicalize(tmp.path()).unwrap();
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec(
                &format!("cd '{}' && exec 2>&1 && echo hi", target.display()),
                Duration::from_secs(10),
                CaptureMode::Stderr,
            )
            .await
            .unwrap();
        // The command's `exec 2>&1` abandoned fd 2 (pointed it at fd 1), so the
        // stderr capture (`err_cap`) saw nothing: stderr is empty.
        assert_eq!(out.stderr.as_deref().unwrap(), "");
        assert!(out.stdout.is_none() && out.merged.is_none());
        // The marker followed the redirect onto stdout; cwd is still recovered
        // via the extract fallback that scans the internally-captured stdout.
        assert_eq!(out.cwd, target);
    }

    /// `Merged` truncation reflects the single combined capture: overflowing it
    /// flags `truncated` and retains the tail (single BoundedCapture semantics,
    /// not an OR of two streams).
    #[tokio::test]
    async fn exec_merged_truncation_single_stream() {
        let mut backend = ShellBuilder::new()
            .max_output_bytes(64)
            .build()
            .await
            .unwrap();
        // Write well over the 64-byte budget; in Merged both fds land in one
        // capture, which clips and keeps its tail. The cwd marker is appended at
        // EXIT *after* the command output, so it can dominate the retained tail
        // -- assert the clip semantics, not the exact tail bytes.
        let out = backend
            .exec(
                "printf 'A%.0s' {1..200}; printf 'B%.0s' {1..200} >&2",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert!(out.truncated, "merged stream should be clipped");
        let merged = out.merged.as_deref().unwrap();
        assert!(
            merged.len() <= 64,
            "tail retained within the byte budget: {}",
            merged.len()
        );
        // The marker (appended at EXIT) is still stripped even from a clipped tail.
        assert!(!merged.contains("__JA_CWD_"));
    }

    // -- CwdMarker unit tests -------------------------------------------------

    #[test]
    fn marker_extract_found_strips_line_and_returns_pwd() {
        let m = CwdMarker {
            nonce: "abc".into(),
        };
        let stderr = "real output\n__JA_CWD_abc__:/tmp\n".to_owned();
        let (clean, pwd) = m.extract(&stderr);
        assert_eq!(pwd.as_deref(), Some("/tmp"));
        assert_eq!(clean, "real output");
    }

    #[test]
    fn marker_extract_absent_returns_none_and_keeps_stderr() {
        let m = CwdMarker {
            nonce: "abc".into(),
        };
        let stderr = "no marker here\n";
        let (clean, pwd) = m.extract(stderr);
        assert!(pwd.is_none());
        assert_eq!(clean, stderr);
    }

    #[test]
    fn marker_extract_at_offset_zero() {
        let m = CwdMarker {
            nonce: "abc".into(),
        };
        // Empty command output: just the trap's leading `\n` + marker line.
        let stderr = "\n__JA_CWD_abc__:/tmp\n";
        let (clean, pwd) = m.extract(stderr);
        assert_eq!(pwd.as_deref(), Some("/tmp"));
        assert_eq!(clean, "");
    }

    #[test]
    fn marker_extract_command_output_without_trailing_newline() {
        let m = CwdMarker {
            nonce: "abc".into(),
        };
        // Command wrote `hi` (no newline); the trap's leading `\n` separates.
        let stderr = "hi\n__JA_CWD_abc__:/tmp\n";
        let (clean, pwd) = m.extract(stderr);
        assert_eq!(pwd.as_deref(), Some("/tmp"));
        assert_eq!(clean, "hi");
    }

    #[test]
    fn marker_extract_preserves_command_trailing_newline() {
        let m = CwdMarker {
            nonce: "abc".into(),
        };
        // Command wrote `hi\n`; the trap adds its own `\n` + marker.
        let stderr = "hi\n\n__JA_CWD_abc__:/tmp\n";
        let (clean, pwd) = m.extract(stderr);
        assert_eq!(pwd.as_deref(), Some("/tmp"));
        assert_eq!(clean, "hi\n");
    }

    #[test]
    fn marker_extract_multiple_matches_takes_last_strips_all() {
        // Defensive: if the same nonce appeared more than once (it cannot in
        // practice — the nonce is random per call), the real emission is last
        // and no marker bytes survive in cleaned stderr. Every trap emission
        // carries a leading newline, so both markers are matched and stripped.
        let m = CwdMarker {
            nonce: "abc".into(),
        };
        let stderr = "\n__JA_CWD_abc__:/a\nreal\n__JA_CWD_abc__:/b\n";
        let (clean, pwd) = m.extract(stderr);
        assert_eq!(pwd.as_deref(), Some("/b"));
        assert_eq!(clean, "real");
        assert!(!clean.contains("__JA_CWD_"));
    }

    #[test]
    fn marker_extract_never_leaves_needle_in_cleaned() {
        // Property: for any realistic stderr (every marker carries the trap's
        // leading newline), the cleaned result contains no marker bytes.
        for stderr in [
            "x\n__JA_CWD_abc__:/p\ny\n__JA_CWD_abc__:/q\n",
            "\n__JA_CWD_abc__:/only\n",
            "\n__JA_CWD_abc__:/nopath\n trailing",
            "no marker",
        ] {
            let m = CwdMarker {
                nonce: "abc".into(),
            };
            let (clean, _) = m.extract(stderr);
            assert!(
                !clean.contains("__JA_CWD_"),
                "cleaned leaked marker: {clean:?}"
            );
        }
    }

    #[test]
    fn marker_roundtrip_via_trap_script_shape() {
        // The trap emits the exact needle shape `extract` scans for, so a
        // synthesized emission round-trips. The needle already carries the
        // leading newline; the payload + terminating newline follow.
        let m = CwdMarker::new();
        let emitted = format!("{}/srv/work\n", m.needle());
        let (clean, pwd) = m.extract(&emitted);
        assert_eq!(pwd.as_deref(), Some("/srv/work"));
        assert_eq!(clean, "");
    }
}
