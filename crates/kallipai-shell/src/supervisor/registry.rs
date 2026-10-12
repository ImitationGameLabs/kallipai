//! Background-task registry: the record types a task is tracked with, the
//! terminal transition that closes it, and the two-phase kill.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::backend::CaptureMode;
use crate::builder::DEFAULT_DISK_CAP;
use crate::capture::{self, BoundedCapture};
use crate::error::ShellError;
use crate::pgroup;
use crate::spill;

use super::tail::{read_pipes_tail, read_tail, remove_pipes_spill};
use super::watch::{pump_log_chunk, task_reasons, watch};
use super::{
    BgReadOutput, EXIT_NONE, KILL_JOIN, OnTaskTerminal, STATE_KILLED, STATE_RUNNING, TaskId,
    TaskState,
};

struct BackgroundTask {
    /// Where this task's output lives — see [`TaskOutput`].
    output: TaskOutput,
    /// Process-group leader pid (PGID == pid); used to force-kill the whole
    /// group on registry drop, since `Drop` can't await the watcher.
    pid: Option<u32>,
    state: Arc<AtomicU8>,
    exit_code: Arc<AtomicI32>,
    stalled: Arc<AtomicBool>,
    bytes: Arc<AtomicUsize>,
    /// Reason slot for the capped-sink pump (disk cap / poison degrade);
    /// surfaced in the read body.
    cap_reason: Arc<Mutex<Option<String>>>,
    cancel: CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// Shared mutable state observed by both the watcher task and read/kill.
pub(crate) struct Watched {
    pub(crate) state: Arc<AtomicU8>,
    pub(crate) exit_code: Arc<AtomicI32>,
    pub(crate) stalled: Arc<AtomicBool>,
    pub(crate) bytes: Arc<AtomicUsize>,
}

/// Where a background task's output lives.
///
/// `File` (spawned tasks): both pipes were redirected into one `out.log`
/// under the task's private `bg/<task-id>/` spill-tree dir before the fork.
/// `Pipes` (adopted tasks —
/// a timed-out foreground exec converted to background): the original pipe
/// pumps and their bounded captures keep running, so the partial output
/// survives the conversion and keeps growing. Cloned into the watcher (the
/// `Arc`s share the live captures); the registry entry keeps the original.
#[derive(Clone)]
pub(crate) enum TaskOutput {
    File {
        /// Merged `out.log` path (probed for size/stall; read by `read`).
        path: PathBuf,
    },
    Pipes {
        out: Arc<Mutex<BoundedCapture>>,
        /// `None` under `CaptureMode::Merged` (no stderr pump exists).
        err: Option<Arc<Mutex<BoundedCapture>>>,
        mode: CaptureMode,
    },
}

/// The pipe pump tasks of an adopted task. Owned by the task's watcher,
/// which drains (or aborts) them once the child reaches a terminal state;
/// the registry entry holds only the shared captures.
pub(crate) struct Pumps {
    pub(crate) out: tokio::task::JoinHandle<()>,
    /// `None` under `CaptureMode::Merged`.
    pub(crate) err: Option<tokio::task::JoinHandle<()>>,
}

/// Everything one watcher task needs; consolidates the loose `watch`
/// arguments. `pumps` and `marker_read` are set only for
/// adopted (converted) tasks.
pub(crate) struct WatchArgs {
    pub(crate) child: Child,
    pub(crate) watched: Watched,
    pub(crate) cancel: CancellationToken,
    pub(crate) max_bg_bytes: usize,
    pub(crate) id: String,
    pub(crate) on_terminal: Option<OnTaskTerminal>,
    pub(crate) exec_gate: Option<Arc<crate::gate::ExecGate>>,
    pub(crate) output: TaskOutput,
    /// The spawned task's pump/cap reason slot; `None` for adopted tasks,
    /// whose reasons live in the captures (resolved via `output` instead).
    pub(crate) cap_reason: Option<Arc<Mutex<Option<String>>>>,
    pub(crate) pumps: Option<Pumps>,
    /// Read end of an adopted task's cwd-marker pipe. Must outlive the
    /// child: the exec script's EXIT trap writes `pwd -P` to the inherited
    /// write end at exit, and were this end closed first, that write would
    /// SIGPIPE bash — killing it before its real exit status can be reaped.
    /// Held for the watcher's whole life; the single short line fits the
    /// kernel pipe buffer, so the trap never blocks either. A grandchild
    /// flooding the marker fd keeps writing into the buffer (it blocks once
    /// full) until the task is killed — no lost exit code. (The capture
    /// mechanism's SIGPIPE reset makes this marker discipline load-bearing.)
    pub(crate) marker_read: Option<OwnedFd>,
}

/// Result of [`BackgroundRegistry::adopt`].
pub(crate) enum AdoptOutcome {
    /// The child now runs as the background task with this id.
    Adopted(TaskId),
    /// A carve landed between the exec's fork and the adoption (carve-epoch
    /// mismatch): the child's baked landlock domain predates the carve's
    /// access decision, so it must not gain the unbounded life of a
    /// background task. Every movable piece is handed back for the
    /// caller's kill path — exactly today's timeout behavior.
    Refused {
        child: Child,
        pumps: Pumps,
        marker_read: Option<OwnedFd>,
    },
}

/// Registry of background tasks by id.
pub(crate) struct BackgroundRegistry {
    tasks: HashMap<TaskId, BackgroundTask>,
    shell: OsString,
    max_bg_bytes: usize,
    /// Task-wide captured-output disk cap; the out.log pump enforces it.
    disk_cap: usize,
    env: HashMap<OsString, OsString>,
    on_terminal: Option<OnTaskTerminal>,
    /// When set (Linux + `landlock` feature), each background `bash` is
    /// landlock-restricted to the owning agent's current access decision.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    access_source: Option<crate::builder::AccessSource>,
    /// Per-agent execution gate shared with the tagma. READ is held across each
    /// background spawn's snapshot+fork; each running task also bumps
    /// `running_bg` for its lifetime so a carve-out refuses while it runs.
    exec_gate: Option<Arc<crate::gate::ExecGate>>,
}

impl BackgroundRegistry {
    pub(crate) fn new(
        shell: OsString,
        max_bg_bytes: usize,
        env: HashMap<OsString, OsString>,
        on_terminal: Option<OnTaskTerminal>,
    ) -> Self {
        Self {
            tasks: HashMap::new(),
            shell,
            max_bg_bytes,
            disk_cap: DEFAULT_DISK_CAP,
            env,
            on_terminal,
            #[cfg(all(target_os = "linux", feature = "landlock"))]
            access_source: None,
            exec_gate: None,
        }
    }

    /// Enable landlock enforcement on background tasks using the given
    /// access-decision snapshot source (the owning agent's composed decision).
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    pub(crate) fn with_access_source(mut self, source: crate::builder::AccessSource) -> Self {
        self.access_source = Some(source);
        self
    }

    /// Share the per-agent execution gate so background spawns coordinate with
    /// workspace carve-outs (READ across snapshot+fork; `running_bg` for lifetime).
    pub(crate) fn with_exec_gate(mut self, gate: Arc<crate::gate::ExecGate>) -> Self {
        self.exec_gate = Some(gate);
        self
    }

    /// Overrides the task-wide captured-output disk cap (from the builder).
    pub(crate) fn with_disk_cap(mut self, disk_cap: usize) -> Self {
        self.disk_cap = disk_cap;
        self
    }

    /// Spawn `command` as a background task; returns its id.
    pub(crate) async fn spawn(&mut self, command: &str) -> Result<TaskId, ShellError> {
        let id = uuid::Uuid::new_v4().to_string();
        // Each task owns a private output dir under the spill tree —
        // `spill_root()/bg/<task-id>/out.log`. No in-process cleanup: the
        // system /tmp mechanisms own the lifecycle, like every spill file.
        // No cwd EXIT trap: background must not touch the shared sticky cwd.
        let task_dir = spill::bg_task_dir(&id)?;
        let output_path = task_dir.join("out.log");

        // Create the output file (0600 via `O_EXCL | O_NOFOLLOW` dirfd) so
        // the redirect target exists before spawn.
        let output = spill::create_private_file(&task_dir, "out.log")?;

        let mut cmd = Command::new(&self.shell);
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        // Apply builder env (parity with foreground exec) + color suppression.
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        for (key, value) in crate::backend::COLOR_VARS {
            cmd.env(key, value);
        }
        // Hold the exec-gate READ across the landlock snapshot + fork so a
        // concurrent workspace carve-out (WRITE on the tagma) cannot interleave:
        // the snapshot below cannot be preceded by a carve that narrows the
        // writable set. No-op when no gate is configured.
        let _bg_gate = crate::gate::ExecGate::read(&self.exec_gate).await;
        // Landlock-restrict the background bash to the agent's access decision
        // (Linux + landlock). Compose the decision (lock-manager-backed snapshot
        // + this task's own bg/<task-id> dir as scratch) via `AccessSource`; `apply` is
        // pure mechanism — it moves the prepared landlock/mount-hole state into
        // the `pre_exec` closure held by `cmd` until `spawn()` consumes it.
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        if let Some(source) = &self.access_source {
            crate::landlock::apply(&mut cmd, &source.access_with_scratch(&task_dir)?)?;
        }
        #[cfg(unix)]
        // Restore the child tree's default SIGPIPE disposition: when the
        // capture pump closes the pipes at the disk cap, the writer dies at
        // its next write instead of spinning on EPIPE.
        crate::pgroup::reset_sigpipe(&mut cmd);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        // Capped-sink pump: both streams are pipes now. One task serializes
        // them into out.log and enforces the task-wide disk cap with a
        // pre-write check — out.log never exceeds the cap. At the cap (or on
        // a log write failure) the pump drops the read ends: the child's
        // next write dies on SIGPIPE (default disposition restored above) or
        // fails with EPIPE, and the reason rides to read/finish through
        // `cap_reason`.
        let cap_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let quota = capture::DiskQuota::new(self.disk_cap);
        let log = tokio::fs::File::from_std(output);
        let mut out_read = child.stdout.take();
        let mut err_read = child.stderr.take();
        let pump_reason = cap_reason.clone();
        let _log_pump = tokio::spawn(async move {
            let mut log = log;
            let mut out_buf = [0u8; 8 * 1024];
            let mut err_buf = [0u8; 8 * 1024];
            let mut out_open = out_read.is_some();
            let mut err_open = err_read.is_some();
            let mut flowing = true;
            while flowing && (out_open || err_open) {
                tokio::select! {
                    read = async {
                        match out_read.as_mut() {
                            Some(r) => r.read(&mut out_buf).await,
                            None => std::future::pending().await,
                        }
                    }, if out_open => match read {
                        Ok(0) | Err(_) => out_open = false,
                        Ok(n) => {
                            flowing = pump_log_chunk(&mut log, &quota, &pump_reason, &out_buf[..n]).await;
                            if !flowing {
                                out_open = false;
                                err_open = false;
                            }
                        }
                    },
                    read = async {
                        match err_read.as_mut() {
                            Some(r) => r.read(&mut err_buf).await,
                            None => std::future::pending().await,
                        }
                    }, if err_open => match read {
                        Ok(0) | Err(_) => err_open = false,
                        Ok(n) => {
                            flowing = pump_log_chunk(&mut log, &quota, &pump_reason, &err_buf[..n]).await;
                            if !flowing {
                                out_open = false;
                                err_open = false;
                            }
                        }
                    },
                }
            }
            // Dropping out_read/err_read here closes the pipe read ends: a
            // live child's next write dies on SIGPIPE / fails with EPIPE.
        });
        // Bump the running-bg tally WHILE the READ permit is still held and
        // BEFORE the watcher is spawned. Two races this closes:
        //  - Dropping the permit before inc would leave a window where a
        //    concurrent carve sees permit-free + counter-0 and narrows the
        //    writable set while this just-forked child retains its baked
        //    (broader) domain -- exactly the snapshot TOCTOU the gate prevents.
        //  - inc must happen-before the watcher's terminal dec; `tokio::spawn`
        //    (next) establishes that ordering for this write, so the watcher
        //    always observes counter >= 1 before it can dec.
        // `inc_guard` undoes the inc on an unwind before the watcher is spawned
        // (the only window with no dec path); disarmed once the watcher owns it.
        let mut inc_guard = if let Some(g) = &self.exec_gate {
            g.inc_bg();
            Some(BgIncGuard::new(g.clone()))
        } else {
            None
        };
        drop(_bg_gate);

        let state = Arc::new(AtomicU8::new(STATE_RUNNING));
        let exit_code = Arc::new(AtomicI32::new(EXIT_NONE));
        let stalled = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();

        let output = TaskOutput::File {
            path: output_path.clone(),
        };
        let handle = tokio::spawn(watch(WatchArgs {
            child,
            watched: Watched {
                state: state.clone(),
                exit_code: exit_code.clone(),
                stalled: stalled.clone(),
                bytes: bytes.clone(),
            },
            cancel: cancel.clone(),
            max_bg_bytes: self.max_bg_bytes,
            id: id.clone(),
            on_terminal: self.on_terminal.clone(),
            exec_gate: self.exec_gate.clone(),
            output: output.clone(),
            cap_reason: Some(cap_reason.clone()),
            pumps: None,
            marker_read: None,
        }));
        // The watcher now owns the dec via `mark_terminal`; disarm the unwind
        // guard so a later `tasks.insert` unwind does not double-dec.
        if let Some(g) = &mut inc_guard {
            g.disarm();
        }

        self.tasks.insert(
            id.clone(),
            BackgroundTask {
                output,
                pid,
                state,
                exit_code,
                stalled,
                bytes,
                cap_reason,
                cancel,
                handle: Some(handle),
            },
        );
        Ok(id)
    }

    /// Adopt a still-running foreground child (its exec timed out) as a full
    /// background task: same registry, same watchdogs, same read/kill
    /// surface. The child, its pipe pumps, the bounded captures, and the
    /// cwd-marker read end all move in untouched, so every byte captured
    /// before the timeout survives and the streams keep flowing.
    ///
    /// The single `await` (the gate READ) is the carve-epoch close: the exec
    /// forked under `epoch_at_fork`; if any carve landed since (the epoch
    /// moved), the child's baked landlock domain is older than the carve's
    /// access decision and must not gain the unbounded life of a background
    /// task — everything is handed back in [`AdoptOutcome::Refused`] for the
    /// caller's kill path (exactly today's timeout behavior). A cancellation
    /// at that await is equally safe: the caller's kill guard is still
    /// armed, so the group dies either way. Note the READ is acquired on a
    /// cloned `Arc` so the permit borrows a local — the registration below
    /// needs `&mut self`.
    ///
    /// After the epoch check the registration is synchronous and mirrors
    /// `spawn`'s tail exactly (inc_bg under the permit, watcher owns the dec
    /// via `mark_terminal`, unwind guard disarmed once the watcher exists),
    /// so there is no window where the task is half-registered and no path
    /// that leaks the running-bg tally.
    pub(crate) async fn adopt(
        &mut self,
        child: Child,
        marker_read: Option<OwnedFd>,
        pumps: Pumps,
        output: TaskOutput,
        epoch_at_fork: Option<u64>,
    ) -> AdoptOutcome {
        let gate = self.exec_gate.clone();
        let _gate = crate::gate::ExecGate::read(&gate).await;
        if let Some(g) = &gate
            && Some(g.carve_epoch()) != epoch_at_fork
        {
            return AdoptOutcome::Refused {
                child,
                pumps,
                marker_read,
            };
        }
        let id = uuid::Uuid::new_v4().to_string();
        let pid = child.id();
        let state = Arc::new(AtomicU8::new(STATE_RUNNING));
        let exit_code = Arc::new(AtomicI32::new(EXIT_NONE));
        let stalled = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        // Same inc/disarm protocol as `spawn`: the watcher owns the dec.
        let mut inc_guard = if let Some(g) = &self.exec_gate {
            g.inc_bg();
            Some(BgIncGuard::new(g.clone()))
        } else {
            None
        };
        let handle = tokio::spawn(watch(WatchArgs {
            child,
            watched: Watched {
                state: state.clone(),
                exit_code: exit_code.clone(),
                stalled: stalled.clone(),
                bytes: bytes.clone(),
            },
            cancel: cancel.clone(),
            max_bg_bytes: self.max_bg_bytes,
            id: id.clone(),
            on_terminal: self.on_terminal.clone(),
            exec_gate: self.exec_gate.clone(),
            output: output.clone(),
            cap_reason: None,
            pumps: Some(pumps),
            marker_read,
        }));
        if let Some(g) = &mut inc_guard {
            g.disarm();
        }
        self.tasks.insert(
            id.clone(),
            BackgroundTask {
                output,
                pid,
                state,
                exit_code,
                stalled,
                bytes,
                cap_reason: Arc::new(Mutex::new(None)),
                cancel,
                handle: Some(handle),
            },
        );
        AdoptOutcome::Adopted(id)
    }

    /// Read a background task's accumulated output and status.
    pub(crate) fn read(&self, id: &str, tail_bytes: usize) -> Result<BgReadOutput, ShellError> {
        let task = self
            .tasks
            .get(id)
            .ok_or_else(|| ShellError::task_not_found(id))?;
        let bytes = task.bytes.load(Ordering::Relaxed);
        let output = match &task.output {
            TaskOutput::File { path } => read_tail(path, tail_bytes)?,
            TaskOutput::Pipes { out, err, mode } => {
                read_pipes_tail(out, err.as_ref(), *mode, tail_bytes)
            }
        };
        // Disk-cap and poison reasons ride the read body: a tail window may
        // not include the on-disk truncation marker, so the text itself
        // carries why the stream stopped. An Exited+reason (cap) stays
        // distinct from Killed (watchdog) via `state`. The same reasons are
        // exposed structurally on `reason` for field-level consumers.
        let reasons = task_reasons(&task.output, Some(&task.cap_reason));
        let output = if reasons.is_empty() {
            output
        } else {
            format!("{}\n{output}", reasons.join("\n"))
        };
        let code = task.exit_code.load(Ordering::Relaxed);
        Ok(BgReadOutput {
            output,
            state: TaskState::from_u8(task.state.load(Ordering::Relaxed)),
            exit_code: (code != EXIT_NONE).then_some(code),
            stalled: task.stalled.load(Ordering::Relaxed),
            bytes,
            reason: reasons.first().cloned(),
        })
    }

    /// Cancel and reap a background task. Its output dir under the spill
    /// tree is NOT removed — the system /tmp cleanup owns the tree's
    /// lifecycle; post-kill `out.log` reads stay fallible as before.
    pub(crate) async fn kill(&mut self, id: &str) -> Result<(), ShellError> {
        let mut task = self
            .tasks
            .remove(id)
            .ok_or_else(|| ShellError::task_not_found(id))?;
        task.cancel.cancel();
        if let Some(handle) = task.handle.take() {
            let _ = tokio::time::timeout(KILL_JOIN, handle).await;
        }
        // The watcher's cancel branch normally marks the task terminal (and
        // decrements the running-bg tally). But on a KILL_JOIN timeout the
        // JoinHandle is detached and the task is already removed from
        // `self.tasks` (above), so the Drop drain cannot cover it. Settle the
        // tally here as a fallback -- CAS-guarded, so it is a no-op if the
        // watcher already reported terminal.
        mark_terminal(&task.state, STATE_KILLED, self.exec_gate.as_ref());
        remove_pipes_spill(&task.output);
        // `task` drops here: nothing is removed — `out.log` persists under
        // `bg/<task-id>/` for the system cleanup. The watcher has already
        // finished (we awaited the handle above), so nothing reads it again
        // through the registry.
        Ok(())
    }
}

impl Drop for BackgroundRegistry {
    fn drop(&mut self) {
        // For each task: force-kill the whole process group (sync — `Drop`
        // can't await the watcher); the output dirs stay in place for the
        // system /tmp cleanup. `kill_on_drop` alone would only signal the
        // leader, orphaning its children. The orphaned watcher's `out.log`
        // reads are already fallible, so a mid-poll disappearance is
        // tolerated.
        //
        // Also settle the running-bg tally for any task still STATE_RUNNING
        // (a watcher that never reported — e.g. runtime shutting down before it
        // was scheduled). `mark_terminal` CAS-guards the decrement, so a racing
        // watcher that does report terminal first wins and this is a no-op.
        for (_, task) in self.tasks.drain() {
            task.cancel.cancel();
            mark_terminal(&task.state, STATE_KILLED, self.exec_gate.as_ref());
            remove_pipes_spill(&task.output);
            if let Some(pid) = task.pid {
                pgroup::force_kill_group(pid as i32);
            }
        }
    }
}

/// Undoes an `inc_bg` if a background spawn unwinds BEFORE the watcher (which
/// owns the `dec_bg`) is spawned -- the one window where no path would
/// otherwise decrement the tally for this task, permanently refusing future
/// carves. Disarmed ([`Self::disarm`]) as soon as `tokio::spawn(watch)`
/// succeeds: from then on the watcher owns the decrement via `mark_terminal`,
/// even if the later `tasks.insert` unwinds.
struct BgIncGuard(Option<Arc<crate::gate::ExecGate>>);

impl BgIncGuard {
    fn new(gate: Arc<crate::gate::ExecGate>) -> Self {
        Self(Some(gate))
    }
    /// Disarm so `Drop` no longer decrements -- call once the watcher owns the dec.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for BgIncGuard {
    fn drop(&mut self) {
        if let Some(g) = self.0.take() {
            g.dec_bg();
        }
    }
}

/// Transition a task to `terminal`, decrementing the running-bg tally exactly
/// once. The CAS from `STATE_RUNNING` succeeds for only the first caller across
/// the four `watch()` terminal branches, a `kill()`-triggered cancel, and the
/// registry `Drop` drain, so `dec_bg` fires at most once per task. A no-op when
/// no gate is configured. `Relaxed` suffices: the inc→dec happens-before is
/// established by `tokio::spawn` (inc precedes the spawn; the watcher runs only
/// after), not by this atomic's ordering, and `running_bg` itself is `Relaxed`.
pub(crate) fn mark_terminal(
    state: &AtomicU8,
    terminal: u8,
    gate: Option<&Arc<crate::gate::ExecGate>>,
) {
    if state
        .compare_exchange(
            STATE_RUNNING,
            terminal,
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_ok()
        && let Some(g) = gate
    {
        g.dec_bg();
    }
}
