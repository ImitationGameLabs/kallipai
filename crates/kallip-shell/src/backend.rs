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
//! cwd or workspace. On timeout the still-running child is CONVERTED to a
//! background task (the same registry `background:true` uses): exec returns
//! `timed_out: true` plus the new `task_id` and a peek of the output so
//! far, and the agent polls `bash_background_read` / kills via
//! `bash_background_kill` as it judges. The conversion is gated on the
//! exec-gate carve epoch (see `BackgroundRegistry::adopt`): a workspace
//! carve that landed while the command ran refuses the conversion and the
//! child is killed instead (SIGTERM -> grace -> SIGKILL), so no long-lived
//! task ever runs a landlock domain older than the carve's access
//! decision. If the future is dropped before completion (the runtime
//! cancels the tool call), a `GroupKillGuard` force-kills the whole group
//! so grandchildren do not survive the leader — including a cancellation
//! racing the conversion itself. The trap fires on normal exit, `exit`, and
//! SIGTERM, so the sticky cwd is read fresh after every completed command;
//! a converted task does NOT advance it (the command has not finished),
//! and a SIGKILL before the trap (or a kill of a command flooding the
//! marker fd) loses the cwd and the caller falls back (never a stale
//! path). A grandchild that the command intentionally backgrounded and
//! detached on the *normal* exit path (e.g. `sleep 99 & disown; exit`) is
//! not killed -- that is an intentional non-goal (use `spawn_background`
//! for durable background work); only the cancel path force-kills the
//! group.

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
    /// Process exit code, or `None` on signal death. `None` while a
    /// timed-out command keeps running as a background task (`task_id` is
    /// set — poll the task for the eventual code).
    pub exit_code: Option<i32>,
    /// Whether the command exceeded its timeout. When `true`, the command
    /// was converted to a background task (`task_id` set) — unless a
    /// concurrent workspace carve refused the conversion, in which case it
    /// was killed.
    pub timed_out: bool,
    /// Whether a returned stream was clipped (exceeded the byte budget). Only
    /// the stream(s) the mode returns are considered; clipping a discarded
    /// stream is not reported.
    pub truncated: bool,
    /// The working directory after the command (read fresh from the cwd fd
    /// channel).
    pub cwd: PathBuf,
    /// Set when a timeout converted this exec into a background task;
    /// poll `read_background` / `kill_background` with it. `None` for
    /// every completed command (and for refused conversions).
    pub task_id: Option<String>,
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

        // Hold the exec-gate READ across the snapshot+fork -- the only window
        // where a concurrent workspace carve-out could race: the snapshot below
        // reads lock state, and the fork bakes it into a landlock domain. A carve
        // (WRITE) cannot interleave with this READ. Released right after the fork:
        // the running command never re-snapshots, so a carve landing later in this
        // exec is safe (the documented one-command overlap; long-lived background
        // tasks are refused separately via the gate's running-bg counter). This
        // narrow bracket is load-bearing for agent-initiated spawns -- an agent
        // spawns a subagent by running `kallip subagent spawn` IN a bash_exec, so
        // the spawn request arrives while this exec is past its fork and the gate
        // is free; a whole-exec bracket would deadlock every such spawn. No-op
        // when no gate is configured.
        // Converted-timeout close: a timed-out exec adopting its child as a
        // background task re-checks the gate's carve epoch under a fresh
        // READ (see `BackgroundRegistry::adopt`), so a carve landing AFTER
        // this fork refuses the adoption and the child is killed — a
        // background task never runs a domain older than the carve's access
        // decision. The remaining accepted tradeoff is the pre-existing
        // one-command overlap: a carve landing just before this fork is
        // baked in for this command's life (bounded by its timeout, or by
        // the passed epoch check after a conversion). The orphan non-goal
        // stands: a command that detached a grandchild (`cmd &` /
        // `disown`) leaves it on the pre-carve (broader) domain for its
        // whole life — documented at module top.
        let _exec_gate = crate::gate::ExecGate::read(&self.config.exec_gate).await;
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
        // Epoch at fork (still under the READ permit): the conversion path
        // compares this against the gate's current carve epoch under a
        // fresh READ, to refuse adoption if a carve landed in between.
        let epoch_at_fork = self.config.exec_gate.as_ref().map(|g| g.carve_epoch());
        drop(_exec_gate);
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

        let wait = run_until_exit_or_timeout(&mut child, timeout_dur).await;

        match wait {
            WaitOutcome::Exited(exit_status) => {
                // Abort any still-blocked pump (a grandchild may hold the
                // write-end) and finalize whatever was buffered — partial
                // output is preserved.
                let out_cap = finish_capture(out_task, out_cap).await;
                let err_cap = match err_task {
                    Some(task) => finish_capture(task, err_cap).await,
                    // No stderr pump (Merged): empty, untruncated capture placeholder.
                    None => capture::CaptureResult::default(),
                };

                // A drained-but-discarded stream (Stdout discards stderr,
                // Stderr discards stdout) is never surfaced, so unlink its
                // spill file if it overflowed -- otherwise it would leak
                // under spill_dir with no banner pointing at it.
                match capture {
                    CaptureMode::Stdout => drop_spill(&err_cap),
                    CaptureMode::Stderr => drop_spill(&out_cap),
                    _ => {}
                }

                // Recover the post-command cwd from the private fd channel
                // (the EXIT trap wrote `pwd -P` to it). An absent/empty
                // result (SIGKILL before the trap, or a flooded marker fd,
                // or the probe never came up) falls back -- always an
                // existing dir, never a stale path.
                let pwd = cwd_probe.and_then(CwdProbe::read_cwd);
                let new_cwd = match pwd {
                    Some(p) => cwd::resolve_str(&p, &self.config.fallback_cwd),
                    None => self.config.fallback_cwd.clone(),
                };
                self.cwd = new_cwd.clone();

                let exit_code = exit_status.and_then(|s| s.code());

                // The child has settled and the pumps are drained -- disarm
                // so the guard does not fire a redundant group kill when the
                // future otherwise finishes dropping.
                kill_guard.disarm();

                // Surface only the field(s) the capture mode returns,
                // banner-prefixed on clip (see `stream_fields`).
                let (merged, stdout, stderr, truncated) =
                    stream_fields(capture, &out_cap, &err_cap);
                Ok(ShellOutput {
                    merged,
                    stdout,
                    stderr,
                    exit_code,
                    timed_out: false,
                    truncated,
                    cwd: new_cwd,
                    task_id: None,
                })
            }
            WaitOutcome::TimedOut => {
                // Convert the still-running child into a background task
                // instead of killing it: the agent gets the task id and a
                // peek of the output so far, then polls/kills as it judges.
                // The child, its pumps, the captures, and the cwd-marker
                // read end all move into the task — nothing may be dropped
                // here first: closing the marker read end would SIGPIPE
                // bash at its EXIT trap, losing the exit code. The sticky
                // cwd does NOT advance (the command is unfinished); it
                // stays at its pre-command value.
                //
                // A drained-but-discarded stream's spill is unlinked on the
                // LIVE capture before ownership moves (same rationale as
                // the exited path; the discarded stream's pump keeps
                // writing an anonymous inode, so no disk space is held once
                // it closes).
                match capture {
                    CaptureMode::Stdout => drop_spill_live(&err_cap),
                    CaptureMode::Stderr => drop_spill_live(&out_cap),
                    _ => {}
                }
                let output = supervisor::TaskOutput::Pipes {
                    out: out_cap.clone(),
                    // Merged has no stderr pump (nothing to read).
                    err: (capture != CaptureMode::Merged).then(|| err_cap.clone()),
                    mode: capture,
                };
                match self
                    .background
                    .adopt(
                        child,
                        cwd_probe.map(CwdProbe::into_read),
                        supervisor::Pumps {
                            out: out_task,
                            err: err_task,
                        },
                        output,
                        epoch_at_fork,
                    )
                    .await
                {
                    supervisor::AdoptOutcome::Adopted(task_id) => {
                        // The task owns the lifecycle from here (cancel
                        // token, watcher, registry Drop): disarm so
                        // returning does not fire a redundant group kill.
                        kill_guard.disarm();
                        let out_peek = peek_cap(&out_cap);
                        let err_peek = peek_cap(&err_cap);
                        let (merged, stdout, stderr, truncated) =
                            stream_fields(capture, &out_peek, &err_peek);
                        Ok(ShellOutput {
                            merged,
                            stdout,
                            stderr,
                            // No exit yet: the command is still running
                            // under the returned task id.
                            exit_code: None,
                            timed_out: true,
                            truncated,
                            cwd: self.cwd.clone(),
                            task_id: Some(task_id),
                        })
                    }
                    supervisor::AdoptOutcome::Refused {
                        mut child,
                        pumps,
                        marker_read,
                    } => {
                        // A carve landed between fork and adoption: kill the
                        // child (exactly today's timeout behavior) — its
                        // baked landlock domain must not gain the unbounded
                        // life of a background task.
                        let cwd_probe = marker_read.map(|read| CwdProbe { read });
                        let _ = pgroup::kill_tree(&mut child).await;
                        let out_cap = finish_capture(pumps.out, out_cap).await;
                        let err_cap = match pumps.err {
                            Some(task) => finish_capture(task, err_cap).await,
                            None => capture::CaptureResult::default(),
                        };
                        match capture {
                            CaptureMode::Stdout => drop_spill(&err_cap),
                            CaptureMode::Stderr => drop_spill(&out_cap),
                            _ => {}
                        }
                        // The trap fired during kill_tree's graceful phase
                        // (if bash honored SIGTERM), so the cwd may be
                        // recoverable — same as the exited path.
                        let pwd = cwd_probe.and_then(CwdProbe::read_cwd);
                        let new_cwd = match pwd {
                            Some(p) => cwd::resolve_str(&p, &self.config.fallback_cwd),
                            None => self.config.fallback_cwd.clone(),
                        };
                        self.cwd = new_cwd.clone();
                        kill_guard.disarm();
                        let note = "[timed out; killed — the command could not be \
                                    converted to a background task because a workspace \
                                    carve-out landed while it ran]\n";
                        let (merged, stdout, stderr, truncated) =
                            stream_fields(capture, &out_cap, &err_cap);
                        Ok(ShellOutput {
                            merged: merged.map(|m| format!("{note}{m}")),
                            stdout: stdout.map(|s| format!("{note}{s}")),
                            stderr: stderr.map(|s| format!("{note}{s}")),
                            exit_code: None,
                            timed_out: true,
                            truncated,
                            cwd: new_cwd,
                            task_id: None,
                        })
                    }
                }
            }
        }
    }

    async fn spawn_background(&mut self, command: &str) -> Result<String, ShellError> {
        self.background.spawn(command).await
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

/// Outcome of the foreground wait.
enum WaitOutcome {
    /// The child exited; the status is `None` if the wait itself failed
    /// (treated as a signal death: no exit code).
    Exited(Option<std::process::ExitStatus>),
    /// The timeout elapsed with the child still running — child, pumps, and
    /// captures are all alive and stay owned by the caller, which converts
    /// them into a background task (see `BackgroundRegistry::adopt`).
    TimedOut,
}

/// Wait for `child` to exit naturally, or report that the timeout elapsed
/// with it still running. Nothing is killed here: the timeout branch hands
/// the live child to the conversion path.
async fn run_until_exit_or_timeout(
    child: &mut Child,
    timeout_dur: Duration,
) -> WaitOutcome {
    tokio::select! {
        result = child.wait() => WaitOutcome::Exited(result.ok()),
        _ = tokio::time::sleep(timeout_dur) => WaitOutcome::TimedOut,
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
pub(super) const PUMP_DRAIN_DEADLINE: Duration = Duration::from_secs(1);

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

    /// Release the raw read end. Used when a timed-out exec is converted
    /// into a background task: the read end must move into the task and
    /// outlive the child (see `WatchArgs::marker_read`), instead of being
    /// consumed by `read_cwd` here.
    fn into_read(self) -> OwnedFd {
        self.read
    }
}

/// Render a finalized capture for the LLM: the head+tail view (which already
/// carries a middle-omitted marker when clipped), with a one-line recovery
/// banner prepended when this stream overflowed and spilled. The banner names
/// the spill file once and the `cat` affordance so the model can read the full
/// output back; `stream` matches the JSON field name the model sees.
pub(super) fn with_banner(stream: &str, cap: &capture::CaptureResult) -> String {
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

/// Best-effort unlink of a LIVE (still-pumping) capture's spill file — the
/// discarded-stream twin of `drop_spill`, used on the conversion path
/// where no finalized `CaptureResult` exists yet. The still-running pump
/// keeps appending to the now-anonymous inode; its space is freed when the
/// fd closes at terminal state.
fn drop_spill_live(cap: &Arc<Mutex<capture::BoundedCapture>>) {
    let cap = cap.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(path) = cap.spill_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Poison-tolerant live peek of a shared capture — the conversion envelope
/// renders the output captured so far while the pumps keep writing.
fn peek_cap(cap: &Arc<Mutex<capture::BoundedCapture>>) -> capture::CaptureResult {
    cap.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .peek()
}

/// Assemble exactly the capture mode's stream fields from (finalized or
/// peeked) capture results, prepending the recovery banner to any clipped
/// stream. `truncated` considers just the returned streams — clipping a
/// drained-but-discarded stream is not reported. The streams are pure
/// command output (cwd recovery is off-band), so nothing is stripped.
fn stream_fields(
    capture: CaptureMode,
    out_cap: &capture::CaptureResult,
    err_cap: &capture::CaptureResult,
) -> (Option<String>, Option<String>, Option<String>, bool) {
    let merged = match capture {
        CaptureMode::Merged => Some(with_banner("output", out_cap)),
        _ => None,
    };
    let stdout = match capture {
        CaptureMode::Separate | CaptureMode::Stdout => Some(with_banner("stdout", out_cap)),
        _ => None,
    };
    let stderr = match capture {
        CaptureMode::Separate | CaptureMode::Stderr => Some(with_banner("stderr", err_cap)),
        _ => None,
    };
    let truncated = match capture {
        CaptureMode::Merged | CaptureMode::Stdout => out_cap.truncated,
        CaptureMode::Stderr => err_cap.truncated,
        CaptureMode::Separate => out_cap.truncated || err_cap.truncated,
    };
    (merged, stdout, stderr, truncated)
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
mod tests;
