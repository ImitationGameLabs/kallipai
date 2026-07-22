//! One-shot command execution: a fresh `bash` process per call.
//!
//! Every [`ProcessBackend::exec`] spawns an isolated `bash -c <script>` (piped
//! stdout/stderr, `stdin` null, its own process group). The script rides argv;
//! the post-command cwd rides a **private fd channel** (not the output stream):
//! the parent opens a pipe and dups its write end to a high fd, the script's
//! `EXIT` trap writes `pwd -P` to that fd, and the parent reads it after the
//! child exits. So stdout/stderr carry only the command's own output (no marker
//! to strip, no marker eating the output budget). Output is captured into a
//! bounded head+tail buffer per stream; on overflow the complete stream is
//! spilled to a file under `spill_dir` so the dropped middle is recoverable,
//! and a banner naming the file is prepended to the clipped text. Other than
//! that overflow spill (and only then), `exec` writes nothing under the spawn
//! cwd or workspace. On timeout the whole process group is killed (SIGTERM ->
//! grace -> SIGKILL) and exit code 124 is synthesized. If the future is dropped
//! before completion (the runtime cancels the tool call), a `GroupKillGuard`
//! force-kills the whole group so grandchildren do not survive the leader. The
//! trap fires on normal exit, `exit`, and SIGTERM, so the sticky cwd is read
//! fresh after every command; a SIGKILL before the trap (or a timeout-kill of a
//! command flooding the marker fd) loses the cwd and the caller falls back
//! (never a stale path). A grandchild that the command intentionally
//! backgrounded and detached on the *normal* exit path (e.g. `sleep 99 &
//! disown; exit`) is not killed -- that is an intentional non-goal (use
//! `spawn_background` for durable background work); only the cancel path
//! force-kills the group.

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::unistd::pipe;
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
    /// Only stdout is returned. Stderr is still drained (into a discarded
    /// buffer) so a command that writes heavily to it is not blocked by a full
    /// pipe, but it is not returned.
    Stdout,
    /// Only stderr is returned. Stdout is still drained (into a discarded
    /// buffer) so a command that writes heavily to it is not blocked by a full
    /// pipe, but it is not returned.
    Stderr,
}

/// Result of a command execution. Exactly the [`CaptureMode`]'s output fields
/// are `Some` (`Merged` -> `merged`; `Separate` -> `stdout` + `stderr`; `Stdout` ->
/// `stdout`; `Stderr` -> `stderr`); the rest are `None`, so the tool layer can
/// omitempty-tag them and the caller sees only what it asked for. A clipped
/// stream carries a one-line banner naming the spill file holding its full
/// output. The streams carry only the command's own output: cwd recovery is
/// off-band (a private fd), so nothing is stripped.
#[derive(Debug, Clone, Default)]
pub struct ShellOutput {
    /// Merged stdout+stderr, possibly clipped (head+tail with a middle-omitted
    /// marker) and banner-prefixed on clip. `Some` only under
    /// [`CaptureMode::Merged`].
    pub merged: Option<String>,
    /// Captured stdout, possibly clipped + banner-prefixed on clip. `Some` under
    /// [`CaptureMode::Separate`] or [`CaptureMode::Stdout`].
    pub stdout: Option<String>,
    /// Captured stderr, possibly clipped + banner-prefixed on clip. `Some` under
    /// [`CaptureMode::Separate`] or [`CaptureMode::Stderr`].
    pub stderr: Option<String>,
    /// Process exit code, or `None` on signal death; `Some(124)` on timeout.
    pub exit_code: Option<i32>,
    /// Whether the command exceeded its timeout.
    pub timed_out: bool,
    /// Whether a returned stream was clipped (exceeded the byte budget). Only
    /// the stream(s) the mode returns are considered; clipping a discarded
    /// stream is not reported.
    pub truncated: bool,
    /// The working directory after the command (read fresh from the cwd fd
    /// channel).
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

        // The cwd marker rides a private fd channel (see `CwdProbe`), not the
        // output stream. Set it up before building the script; on failure
        // (exceedingly rare) the trap is omitted and cwd falls back.
        let (cwd_probe, write_end) = match CwdProbe::new() {
            Ok((probe, write_end)) => (Some(probe), Some(write_end)),
            Err(_) => (None, None),
        };
        let marker_fd = write_end.as_ref().map(|w| w.fd());

        // The spill dir is created lazily by the capture on overflow (so an
        // under-budget exec writes nothing to disk); no eager creation here.

        // Build the `-c` script (cwd-trap on the fd channel + command) and reject
        // an oversized script up front with an actionable error.
        let script = build_exec_script(command, marker_fd, capture);
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
        // [`build_exec_script`]): the shell itself points fd 2 at the stdout
        // pipe, so a single stdout pump captures the interleaved stream with
        // program-determined ordering. After the merge no process writes the
        // stderr pipe, so its read-end is dropped below (immediate EOF, no hang).
        // The cwd marker is unaffected -- it rides the separate fd channel.

        // Landlock-restrict this bash to the agent's current access decision
        // (Linux + landlock). The foreground path needs no scratch beyond
        // `baseline_writable` (`/tmp`, `/dev/null`, ...) already folded in by
        // `apply`; the spill file (only on overflow) lands in `spill_dir`, which
        // is `temp_dir()` and thus already in the writable set. `apply` is pure
        // mechanism: it moves the prepared landlock/mount-hole state into the
        // `pre_exec` closure, which `cmd` owns until `spawn()` consumes it, so
        // the ruleset fd survives the fork and is read in the child. The marker
        // fd is inherited independently (CLOEXEC cleared) and is not a filesystem
        // object, so landlock/seccomp do not restrict it.
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        if let Some(source) = &self.config.access_source {
            crate::landlock::apply(&mut cmd, &source.access()?)?;
        }

        let mut child = cmd.spawn()?;
        // The child has forked and inherited the marker fd; release the parent's
        // write-end copy NOW so the read end can reach EOF at child exit (this is
        // load-bearing -- holding it past `read_cwd` would deadlock the read).
        // `Drop` covers an early `?`-return between `CwdProbe::new` and here.
        drop(write_end);

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
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        // Stream label embedded in any spill filename: the merged stream under
        // `Merged`, else the per-pipe `stdout`/`stderr`.
        let out_label = if capture == CaptureMode::Merged {
            "merged"
        } else {
            "stdout"
        };
        // Shared captures so partial output survives even if a pump is stuck
        // (a grandchild holding the pipe write-end) and has to be aborted.
        let out_cap = Arc::new(Mutex::new(capture::BoundedCapture::new(
            max,
            &nonce,
            out_label,
            self.config.spill_dir.clone(),
        )));
        let err_cap = Arc::new(Mutex::new(capture::BoundedCapture::new(
            max,
            &nonce,
            "stderr",
            self.config.spill_dir.clone(),
        )));
        let out_task = tokio::spawn(pump(child.stdout.take(), out_cap.clone()));
        // In Merged mode the script's `exec 2>&1` points fd 2 at the stdout
        // pipe, so the stderr pipe carries nothing: skip its pump and drop the
        // read-end (immediate EOF, no hang). All other modes still pump both
        // streams (the discarded one is drained into a buffer and not returned)
        // so a command that writes heavily to the unreturned stream is not
        // blocked by a full pipe.
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

        // A drained-but-discarded stream (Stdout discards stderr, Stderr
        // discards stdout) is never surfaced, so unlink its spill file if it
        // overflowed -- otherwise it would leak under spill_dir with no banner
        // pointing at it.
        match capture {
            CaptureMode::Stdout => drop_spill(&err_cap),
            CaptureMode::Stderr => drop_spill(&out_cap),
            _ => {}
        }

        // Recover the post-command cwd from the private fd channel (the EXIT
        // trap wrote `pwd -P` to it). An absent/empty result (SIGKILL/timeout
        // before the trap, or a flooded marker fd that the timeout killed, or
        // the probe never came up) falls back -- always an existing dir, never a
        // stale path.
        let pwd = cwd_probe.and_then(CwdProbe::read_cwd);
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

        // Surface only the field(s) the capture mode returns, prepending the
        // recovery banner to any clipped stream. `truncated` considers just the
        // returned streams — clipping a drained-but-discarded stream is not
        // reported. The streams are pure command output (cwd recovery is
        // off-band), so nothing is stripped.
        let merged = match capture {
            CaptureMode::Merged => Some(with_banner("output", &out_cap)),
            _ => None,
        };
        let stdout = match capture {
            CaptureMode::Separate | CaptureMode::Stdout => Some(with_banner("stdout", &out_cap)),
            _ => None,
        };
        let stderr = match capture {
            CaptureMode::Separate | CaptureMode::Stderr => Some(with_banner("stderr", &err_cap)),
            _ => None,
        };
        let truncated = match capture {
            CaptureMode::Merged | CaptureMode::Stdout => out_cap.truncated,
            CaptureMode::Stderr => err_cap.truncated,
            CaptureMode::Separate => out_cap.truncated || err_cap.truncated,
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

// -- foreground cwd-recovery fd channel --------------------------------------

/// Lowest fd number the cwd marker may occupy in the child. A high number
/// avoids colliding with bash's own fds and ordinary user `exec N>...`
/// redirects; the actual fd is chosen by `F_DUPFD` as the lowest free fd at or
/// above this floor. A command that happens to use this exact fd loses the cwd
/// marker and the caller falls back -- recoverable, not a hazard.
const MARKER_FD_FLOOR: RawFd = 63;

/// The read end of the cwd marker pipe. The paired write end is inherited by
/// the spawned `bash` ([`WriteEnd`]); its EXIT trap writes `pwd -P` to it, and
/// the parent reads the result once the child is gone.
struct CwdProbe {
    read: OwnedFd,
}

/// The parent's copy of the marker pipe's write end, duped to a known high fd
/// that the child inherits. Held only until `spawn()` returns, then dropped so
/// the read end can reach EOF; `OwnedFd`'s `Drop` closes it, and is the
/// error-path defense too (an early `?`-return between [`CwdProbe::new`] and
/// the explicit drop never strands a write end). Holding an `OwnedFd` (not a
/// bare `RawFd`) makes sole ownership a type property, so no `unsafe`/manual
/// `Drop` is needed.
struct WriteEnd(OwnedFd);

impl WriteEnd {
    /// The fd the child inherited and the trap writes to.
    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl CwdProbe {
    /// Create the marker pipe, dup the write end to a high fd (CLOEXEC cleared
    /// so the child inherits it), close the original write end, and mark the
    /// read end CLOEXEC (the child must not inherit it). Returns the probe plus
    /// the [`WriteEnd`] guard whose `fd()` the trap script references.
    fn new() -> std::io::Result<(CwdProbe, WriteEnd)> {
        let (read, write) = pipe().map_err(std::io::Error::from)?;
        // F_DUPFD returns the lowest free fd >= floor; it does NOT set CLOEXEC,
        // so the child inherits this dup across execve. Wrap the result in an
        // `OwnedFd` so its `Drop` owns the close.
        let marker_raw = fcntl(write.as_fd(), FcntlArg::F_DUPFD(MARKER_FD_FLOOR))
            .map_err(std::io::Error::from)? as RawFd;
        let marker_fd = unsafe { OwnedFd::from_raw_fd(marker_raw) };
        // Close the original write end (the dup at `marker_fd` survives).
        drop(write);
        // Keep the child from inheriting the read end.
        fcntl(read.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(std::io::Error::from)?;
        Ok((CwdProbe { read }, WriteEnd(marker_fd)))
    }

    /// Read the cwd the EXIT trap wrote. Called after the child is reaped (or
    /// timeout-killed), by which point the trap's single short `pwd -P` line is
    /// already in the kernel pipe buffer.
    ///
    /// The read end is set **nonblocking** so a backgrounded grandchild that
    /// inherited the marker fd (CLOEXEC is cleared so the trap can use it) and
    /// outlives the leader cannot wedge this read: we read whatever is available
    /// right now and stop at `WouldBlock`/EOF, instead of draining to EOF (which
    /// would block until every inherited write-end copy closes). The pwd line is
    /// written atomically (`< PIPE_BUF`), so one read collects it whole. Cap the
    /// read at the pipe buffer size; take the last non-empty line.
    fn read_cwd(self) -> Option<String> {
        // Pipes carry no settable flags besides O_NONBLOCK, so F_SETFL it
        // directly (best-effort: a failure leaves the fd blocking, but the read
        // loop still returns once data is available on the happy path).
        let _ = fcntl(self.read.as_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK));
        let mut file = File::from(self.read);
        let mut buf = Vec::with_capacity(256);
        let mut chunk = [0u8; 4096];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // Cap at the pipe buffer size; the trap's payload is tiny.
                    if buf.len() >= 64 * 1024 {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if buf.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&buf);
        text.lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_owned())
    }
}

/// Render a finalized capture for the LLM: the head+tail view (which already
/// carries a middle-omitted marker when clipped), with a one-line recovery
/// banner prepended when this stream overflowed and spilled. The banner names
/// the spill file once and the `cat` affordance so the model can read the full
/// output back; `stream` matches the JSON field name the model sees.
fn with_banner(stream: &str, cap: &capture::CaptureResult) -> String {
    match &cap.spill {
        Some(path) => {
            let banner = format!(
                "[{stream} was clipped (middle omitted); read the full output with: cat {}]\n",
                path.display()
            );
            format!("{}{}", banner, cap.text)
        }
        None => cap.text.clone(),
    }
}

/// Best-effort unlink of a discarded capture's spill file so it does not leak
/// under `spill_dir` with no banner referencing it.
fn drop_spill(cap: &capture::CaptureResult) {
    if let Some(path) = &cap.spill {
        let _ = std::fs::remove_file(path);
    }
}

/// Build the foreground `-c` script: install the EXIT-trap cwd probe on the
/// private fd channel (if any), then run the command. The whole string is passed
/// as `bash -c`'s single argv element. Under [`CaptureMode::Merged`] an
/// `exec 2>&1` is inserted after the trap so the shell itself merges stderr onto
/// the stdout pipe (program-determined ordering); the cwd trap writes to the fd
/// channel, not fd 2, so it is independent of that merge.
fn build_exec_script(command: &str, marker_fd: Option<RawFd>, capture: CaptureMode) -> String {
    let mut s = String::with_capacity(256 + command.len());
    if let Some(fd) = marker_fd {
        // `pwd -P >&N` duplicates fd N to pwd's stdout for the duration of the
        // call -- a bash fd-dup redirect on the bare integer N (NOT the `>&{N}`
        // brace form, which bash treats as a filename and silently no-ops). It
        // is independent of fds 0/1/2, so `exec 2>&1` / `exec 2>/dev/null` /
        // `exec 1>/dev/null` do not affect cwd recovery.
        s.push_str(&format!(
            "__ja_pwd() {{ pwd -P >&{fd}; }}\ntrap -- __ja_pwd EXIT\n"
        ));
    }
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

    /// Collect all `bash_exec-*.txt` spill files directly under `root` (the spill
    /// layout is flat -- no per-backend subdir).
    fn spill_files(root: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("bash_exec-"))
            })
            .collect()
    }

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
        // cwd is read fresh from the private fd channel after the cd -> sticky.
        assert_eq!(out.cwd, std::fs::canonicalize(&target).unwrap());
        assert!(out.merged.as_deref().unwrap().trim() == out.cwd.to_string_lossy());
        // The cwd marker rides a separate fd, not the output stream, so the
        // merged text is pure command output.
        assert!(!out.merged.as_deref().unwrap().contains("__ja_pwd"));
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
    /// points at the stdout pipe. The cwd marker rides a separate fd channel
    /// (independent of fd 2), so a command that also does its own `exec 2>&1`
    /// still recovers cwd and the merged stream carries only command output.
    #[tokio::test]
    async fn exec_with_merged_stderr_recovers_cwd() {
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
            "cwd must still recover even when the command merges its own streams"
        );
        // Only `merged` is populated; stdout/stderr are None under Merged.
        let merged = out.merged.as_deref().unwrap();
        assert!(out.stdout.is_none() && out.stderr.is_none());
        // The command's own output survives; no marker bytes leak.
        assert!(merged.contains("merged"), "merged: {merged}");
        assert!(!merged.contains("__ja_pwd"));
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

    /// Foreground `exec` writes nothing under the spawn cwd, and an under-budget
    /// exec writes nothing under `spill_dir` either: the script rides argv, the
    /// cwd rides the fd channel, and a spill file appears only on overflow.
    #[tokio::test]
    async fn exec_leaves_no_scratch_in_cwd() {
        let probe = std::env::temp_dir().join(format!(
            "ja-shell-probe-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&probe).unwrap();
        let scratch = tempfile::TempDir::new().unwrap();
        let mut backend = ShellBuilder::new()
            .initial_cwd(probe.clone())
            .spill_dir(scratch.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let out = backend
            .exec("echo hi", Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap();
        // Nothing written under the spawn cwd.
        let mut entries = std::fs::read_dir(&probe).unwrap();
        assert!(
            entries.next().is_none(),
            "foreground exec left files behind in {}",
            probe.display()
        );
        // Under budget: no spill file, no clip, no marker.
        assert!(!out.truncated);
        assert!(!out.merged.as_deref().unwrap().contains("bytes omitted"));
        assert!(
            spill_files(scratch.path()).is_empty(),
            "under-budget exec wrote a spill file"
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

    /// `Stdout` returns only stdout but still recovers the cwd: the marker rides
    /// the private fd channel, so it does not depend on capturing stderr. The
    /// returned `stderr` is `None`.
    #[tokio::test]
    async fn exec_stdout_mode_recovers_cwd() {
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
            "Stdout mode must still recover cwd via the fd channel"
        );
    }

    /// `Stderr` mode under a command that redirects fd 2 onto stdout
    /// (`exec 2>&1`): the redirect points fd 2 at the stdout pipe, so the stderr
    /// capture sees nothing and the returned `stderr` is empty. The cwd is still
    /// recovered -- the marker rides the fd channel, independent of fd 2.
    #[tokio::test]
    async fn exec_stderr_mode_with_command_merge() {
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
        // The command's `exec 2>&1` pointed fd 2 at fd 1, so the stderr capture
        // saw nothing: stderr is empty.
        assert_eq!(out.stderr.as_deref().unwrap(), "");
        assert!(out.stdout.is_none() && out.merged.is_none());
        assert_eq!(out.cwd, target);
    }

    /// `Merged` overflow clips the single combined capture to a head+tail view,
    /// flags `truncated`, prepends the recovery banner, and spills the complete
    /// stream to a file whose contents equal the full emitted output.
    #[tokio::test]
    async fn exec_merged_truncation_single_stream() {
        let scratch = tempfile::TempDir::new().unwrap();
        let mut backend = ShellBuilder::new()
            .max_output_bytes(64)
            .spill_dir(scratch.path().to_path_buf())
            .build()
            .await
            .unwrap();
        // Write well over the 64-byte budget; in Merged both fds land in one
        // capture, which clips to head+tail. The cwd marker rides the fd
        // channel, so it does not pollute the captured stream.
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
            merged.contains("bytes omitted"),
            "head+tail view has the middle-omitted marker: {merged}"
        );
        assert!(
            merged.contains("was clipped (middle omitted)"),
            "banner prepended: {merged}"
        );
        assert!(
            merged.contains("with: cat"),
            "banner carries the cat hint: {merged}"
        );
        // No marker bytes from the fd channel leak into the captured stream.
        assert!(!merged.contains("__ja_pwd"));
        // Exactly one spill file, holding the complete stream.
        let files = spill_files(scratch.path());
        assert_eq!(files.len(), 1, "only one spill file under Merged");
        let spilled = std::fs::read(&files[0]).unwrap();
        // 200 'A's + 200 'B's = the complete emitted output, in some order.
        assert_eq!(spilled.len(), 400);
        assert_eq!(spilled.iter().filter(|&&b| b == b'A').count(), 200);
        assert_eq!(spilled.iter().filter(|&&b| b == b'B').count(), 200);
    }

    /// `Separate` overflow spills each stream to its own file and banners each
    /// clipped stream; the non-clipped stream is clean (no banner).
    #[tokio::test]
    async fn exec_separate_overflow_spills_each_stream() {
        let scratch = tempfile::TempDir::new().unwrap();
        let mut backend = ShellBuilder::new()
            .max_output_bytes(64)
            .spill_dir(scratch.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let out = backend
            .exec(
                "printf 'A%.0s' {1..200}; printf 'B%.0s' {1..200} >&2",
                Duration::from_secs(10),
                CaptureMode::Separate,
            )
            .await
            .unwrap();
        assert!(out.truncated);
        let stdout = out.stdout.as_deref().unwrap();
        let stderr = out.stderr.as_deref().unwrap();
        assert!(stdout.contains("clipped (middle omitted)"));
        assert!(stderr.contains("clipped (middle omitted)"));
        // Two distinct spill files (-stdout / -stderr).
        let spills: Vec<String> = spill_files(scratch.path())
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            spills.len(),
            2,
            "two spill files under Separate: {spills:?}"
        );
        assert!(spills.iter().any(|n| n.ends_with("-stdout.txt")));
        assert!(spills.iter().any(|n| n.ends_with("-stderr.txt")));
    }

    // -- CwdProbe / script-shape / spill-security tests -----------------------

    /// The trap script redirects to the bare-integer fd (`>&63`), not the
    /// `>&{63}` brace form (which bash treats as a filename and silently
    /// no-ops). This shape test would have caught that regression.
    #[test]
    fn build_exec_script_uses_bare_fd_redirect() {
        let s = build_exec_script("cmd", Some(63), CaptureMode::Merged);
        assert!(
            s.contains("pwd -P >&63"),
            "trap must use the bare-integer fd redirect, got: {s}"
        );
        assert!(!s.contains(">&{63}"), "brace form is a silent no-op: {s}");
    }

    /// With no marker fd the script omits the trap entirely (pipe setup failed).
    #[test]
    fn build_exec_script_omits_trap_when_no_fd() {
        let s = build_exec_script("cmd", None, CaptureMode::Merged);
        assert!(!s.contains("__ja_pwd"));
    }

    /// `CwdProbe` round-trips a pwd written to the write end: writing a path
    /// line, dropping the write end, then reading yields the trimmed pwd.
    #[test]
    fn cwd_probe_reads_pwd_from_fd_channel() {
        let (probe, write_end) = CwdProbe::new().unwrap();
        // Write the way the trap would (a path + newline) via a borrowed fd
        // (no ownership transfer), then drop the write end so the read end EOFs.
        let _ = nix::unistd::write(write_end.0.as_fd(), b"/srv/example\n");
        drop(write_end);
        let pwd = probe.read_cwd().unwrap();
        assert_eq!(pwd, "/srv/example");
    }

    /// `read_cwd` does not hang when a write-end copy stays open (a stand-in for
    /// a backgrounded grandchild inheriting the marker fd): the read is
    /// nonblocking and returns whatever the trap wrote, then stops at EAGAIN.
    #[tokio::test]
    async fn cwd_probe_does_not_hang_when_write_end_stays_open() {
        let (probe, write_end) = CwdProbe::new().unwrap();
        let _ = nix::unistd::write(write_end.0.as_fd(), b"/srv/example\n");
        // Do NOT drop write_end -- emulate a grandchild holding the fd open.
        let pwd = tokio::time::timeout(Duration::from_secs(2), async { probe.read_cwd() })
            .await
            .expect("read_cwd must not hang on a held write end");
        assert_eq!(pwd.unwrap(), "/srv/example");
    }

    /// A probe whose write end is dropped without writing yields no cwd (the
    /// trap never fired / SIGKILL before EXIT).
    #[test]
    fn cwd_probe_empty_when_trap_never_fired() {
        let (probe, write_end) = CwdProbe::new().unwrap();
        drop(write_end);
        assert!(probe.read_cwd().is_none());
    }

    /// The marker fd is at or above `MARKER_FD_FLOOR`.
    #[test]
    fn cwd_probe_marker_fd_is_high() {
        let (_probe, write_end) = CwdProbe::new().unwrap();
        assert!(write_end.fd() >= MARKER_FD_FLOOR);
    }

    /// A spilled file is created owner-only (0o600): no info leak to other uids
    /// on a multi-user host.
    #[tokio::test]
    async fn spill_file_is_owner_only() {
        let scratch = tempfile::TempDir::new().unwrap();
        let mut backend = ShellBuilder::new()
            .max_output_bytes(32)
            .spill_dir(scratch.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let out = backend
            .exec(
                "printf 'A%.0s' {1..200}",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        assert!(out.truncated);
        let spill = spill_files(scratch.path()).pop().expect("a spill file");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&spill).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "spill file must be owner-only, got {:o}", mode);
    }

    /// The overflow-time spill open refuses a symlinked `spill_dir`: even if an
    /// adversary swaps the dir for a symlink between build and the first overflow,
    /// the `O_NOFOLLOW` dir open poisons (no banner, no spill path) and the write
    /// does NOT follow the symlink into its target. Guards the TOCTOU.
    #[tokio::test]
    async fn spill_refuses_symlinked_spill_dir() {
        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path().join("dir");
        let target = root.path().join("target");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        // Build against the real dir (passes the build-time check).
        let mut backend = ShellBuilder::new()
            .max_output_bytes(32)
            .spill_dir(dir.clone())
            .build()
            .await
            .unwrap();
        // Swap: replace the real dir with a symlink -> target, then overflow.
        std::fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        let out = backend
            .exec(
                "printf 'A%.0s' {1..200}",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        // Overflow happened but the spill poisoned: no recovery banner.
        assert!(out.truncated, "stream should still be clipped in-memory");
        assert!(
            !out.merged
                .as_deref()
                .unwrap()
                .contains("read the full output with"),
            "no banner when spill_dir is a symlink at overflow time"
        );
        // The write did NOT follow the symlink into the target dir.
        assert!(
            std::fs::read_dir(&target).unwrap().next().is_none(),
            "spill wrote through the symlinked spill_dir into the target"
        );
    }

    /// A SIGKILL before the EXIT trap fires loses the cwd (no trap ran); the
    /// caller falls back rather than reporting a stale path.
    #[tokio::test]
    async fn sigkill_before_trap_falls_back() {
        let mut backend = ShellBuilder::new().build().await.unwrap();
        let out = backend
            .exec("kill -9 $$", Duration::from_secs(10), CaptureMode::Merged)
            .await
            .unwrap();
        assert!(out.cwd.exists(), "cwd must fall back to an existing dir");
    }

    /// A second landlocked `bash` can `cat` a spill file the tagma parent
    /// wrote under `temp_dir()`: `baseline_writable` grants read on writable
    /// paths. Guards the read-back affordance the banner advertises.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    #[tokio::test]
    async fn spilled_file_is_readable_by_landlocked_cat() {
        use crate::landlock;
        if landlock::ensure_supported().is_err() {
            return;
        }
        let scratch = tempfile::TempDir::new().unwrap();
        let mut backend = ShellBuilder::new()
            .max_output_bytes(32)
            .spill_dir(scratch.path().to_path_buf())
            .access_source(|| {
                Ok(landlock::AccessDecision {
                    read: landlock::ReadPolicy::Broad,
                    writable: Vec::new(),
                    readonly_holes: Vec::new(),
                    hide_holes: Vec::new(),
                })
            })
            .build()
            .await
            .unwrap();
        let first = backend
            .exec(
                "printf 'HEAD'; printf 'A%.0s' {1..200}",
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        let merged = first.merged.as_deref().unwrap();
        let path = merged
            .lines()
            .next()
            .and_then(|line| {
                line.split("with: cat ")
                    .nth(1)
                    .and_then(|rest| rest.trim_end_matches(']').trim().to_string().into())
            })
            .expect("banner with a spill path");
        let second = backend
            .exec(
                &format!("cat '{path}'"),
                Duration::from_secs(10),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
        let reread = second.merged.as_deref().unwrap();
        assert!(
            reread.contains("HEAD"),
            "landlocked cat read the spill back"
        );
    }
}
