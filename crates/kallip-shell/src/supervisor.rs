//! Background-process supervisor (Claude-Code style).
//!
//! Each spawned background task is its own `bash` process writing merged
//! stdout/stderr to a file; a timed-out foreground exec is instead
//! *adopted* — its still-running child, pipe pumps, and bounded captures
//! move into the registry untouched, keeping every byte captured before
//! the timeout. Either way a watcher task polls for exit, runs a stall
//! watchdog (quiescence + tail regex) and a size watchdog, and drives a
//! two-phase kill on cancel. Modeled on the tagma's agent registry
//! (`state.rs`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use regex::Regex;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::backend::CaptureMode;
use crate::capture::BoundedCapture;
use crate::error::ShellError;
use crate::pgroup;

/// LLM-facing identifier for a background task (UUID v4 string).
pub(super) type TaskId = String;

const WATCH_POLL: Duration = Duration::from_millis(200);
/// Stall requires this much output-quiescence before the tail regex is trusted,
/// so a build log printing `Compiling foo:` can't trip it (R6).
const STALL_QUIESCENCE: Duration = Duration::from_secs(3);
/// Tail size examined for interactive-prompt lockups.
const STALL_TAIL: u64 = 4 * 1024;
/// Bounded wait for a watcher task to finish after cancel.
const KILL_JOIN: Duration = Duration::from_secs(5);

const EXIT_NONE: i32 = -1;
const STATE_RUNNING: u8 = 0;
const STATE_EXITED: u8 = 1;
const STATE_KILLED: u8 = 2;

/// Visible task state (serialized for the LLM as a string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Exited,
    Killed,
}

impl TaskState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Exited,
            2 => Self::Killed,
            _ => Self::Running,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
        }
    }
}

/// Observer invoked when a background task reaches a terminal state. Receives
/// `(task_id, state, exit_code)`; `exit_code` is `None` for killed / watcher-error
/// cases. Best-effort: may not fire on registry `Drop` — the runtime may be
/// shutting down and the watcher cannot be awaited synchronously, so callers must
/// tolerate a missed notification (equivalent to the task being reclaimed).
pub type OnTaskTerminal = Arc<dyn Fn(&str, TaskState, Option<i32>) + Send + Sync>;

/// Owned terminal-state observer with a `Debug` impl (trait objects have none),
/// so it can live in a `#[derive(Debug)]` struct like `ShellBuilder`.
#[derive(Clone)]
pub(super) struct TerminalObserver(pub(super) OnTaskTerminal);

impl std::fmt::Debug for TerminalObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalObserver").finish_non_exhaustive()
    }
}

/// Result of reading a background task's accumulated output.
#[derive(Debug)]
pub struct BgReadOutput {
    /// Tail of the task's accumulated output (its log file for a spawned
    /// task, or the live captured streams of a converted one).
    pub output: String,
    /// Current task state.
    pub state: TaskState,
    /// Exit code once the task has exited, else `None`.
    pub exit_code: Option<i32>,
    /// `true` if the task appears stalled on an interactive prompt.
    pub stalled: bool,
    /// Total bytes written so far.
    pub bytes: usize,
}

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
    cancel: CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
    /// A spawned task's auto-cleaned output dir (`out.log` lives here);
    /// `None` for converted tasks, whose spill files are unlinked on
    /// kill/drop instead (see `remove_pipes_spill`). Held for its `Drop`
    /// side-effect (the dir is removed when the task is dropped), never read
    /// directly. The watcher task itself is a separate tokio task that may
    /// outlive this struct, but its `out.log` reads are already fallible
    /// (`unwrap_or(0)`/`read_tail`), so a dir removed mid-poll is tolerated —
    /// identical to the old manual `remove_dir_all` cleanup, no new
    /// dangling-read path. Drop order is irrelevant to that safety.
    #[allow(dead_code)]
    tmpdir: Option<tempfile::TempDir>,
}

/// Shared mutable state observed by both the watcher task and read/kill.
struct Watched {
    state: Arc<AtomicU8>,
    exit_code: Arc<AtomicI32>,
    stalled: Arc<AtomicBool>,
    bytes: Arc<AtomicUsize>,
}

/// Where a background task's output lives.
///
/// `File` (spawned tasks): both pipes were redirected into one `out.log` in
/// the task's auto-cleaned tmpdir before the fork. `Pipes` (adopted tasks —
/// a timed-out foreground exec converted to background): the original pipe
/// pumps and their bounded captures keep running, so the partial output
/// survives the conversion and keeps growing. Cloned into the watcher (the
/// `Arc`s share the live captures); the registry entry keeps the original.
#[derive(Clone)]
pub(super) enum TaskOutput {
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
pub(super) struct Pumps {
    pub(super) out: tokio::task::JoinHandle<()>,
    /// `None` under `CaptureMode::Merged`.
    pub(super) err: Option<tokio::task::JoinHandle<()>>,
}

/// Everything one watcher task needs; converges what used to be eight
/// loose `watch` arguments. `pumps` and `marker_read` are set only for
/// adopted (converted) tasks.
struct WatchArgs {
    child: Child,
    watched: Watched,
    cancel: CancellationToken,
    max_bg_bytes: usize,
    id: String,
    on_terminal: Option<OnTaskTerminal>,
    exec_gate: Option<Arc<super::gate::ExecGate>>,
    output: TaskOutput,
    pumps: Option<Pumps>,
    /// Read end of an adopted task's cwd-marker pipe. Must outlive the
    /// child: the exec script's EXIT trap writes `pwd -P` to the inherited
    /// write end at exit, and were this end closed first, that write would
    /// SIGPIPE bash — killing it before its real exit status can be reaped.
    /// Held for the watcher's whole life; the single short line fits the
    /// kernel pipe buffer, so the trap never blocks either. A grandchild
    /// flooding the marker fd keeps writing into the buffer (it blocks once
    /// full) until the task is killed — no SIGPIPE, no lost exit code.
    marker_read: Option<OwnedFd>,
}

/// Result of [`BackgroundRegistry::adopt`].
pub(super) enum AdoptOutcome {
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
pub(super) struct BackgroundRegistry {
    tasks: HashMap<TaskId, BackgroundTask>,
    shell: OsString,
    max_bg_bytes: usize,
    env: HashMap<OsString, OsString>,
    on_terminal: Option<OnTaskTerminal>,
    /// When set (Linux + `landlock` feature), each background `bash` is
    /// landlock-restricted to the owning agent's current access decision.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    access_source: Option<super::builder::AccessSource>,
    /// Per-agent execution gate shared with the tagma. READ is held across each
    /// background spawn's snapshot+fork; each running task also bumps
    /// `running_bg` for its lifetime so a carve-out refuses while it runs.
    exec_gate: Option<Arc<super::gate::ExecGate>>,
}

impl BackgroundRegistry {
    pub(super) fn new(
        shell: OsString,
        max_bg_bytes: usize,
        env: HashMap<OsString, OsString>,
        on_terminal: Option<OnTaskTerminal>,
    ) -> Self {
        Self {
            tasks: HashMap::new(),
            shell,
            max_bg_bytes,
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
    pub(super) fn with_access_source(mut self, source: super::builder::AccessSource) -> Self {
        self.access_source = Some(source);
        self
    }

    /// Share the per-agent execution gate so background spawns coordinate with
    /// workspace carve-outs (READ across snapshot+fork; `running_bg` for lifetime).
    pub(super) fn with_exec_gate(mut self, gate: Arc<super::gate::ExecGate>) -> Self {
        self.exec_gate = Some(gate);
        self
    }

    /// Spawn `command` as a background task; returns its id.
    pub(super) async fn spawn(&mut self, command: &str) -> Result<TaskId, ShellError> {
        let id = uuid::Uuid::new_v4().to_string();
        // Each task owns an auto-cleaned tmpdir holding its merged `out.log`.
        // No cwd EXIT trap: background must not touch the shared sticky cwd.
        let tmpdir = tempfile::TempDir::new()?;
        let output_path = tmpdir.path().join("out.log");

        // Create the output file so the redirect target exists before spawn.
        let output = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output_path)?;

        let mut cmd = Command::new(&self.shell);
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .process_group(0)
            .kill_on_drop(true);
        // Apply builder env (parity with foreground exec) + color suppression.
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        for (key, value) in super::backend::COLOR_VARS {
            cmd.env(key, value);
        }
        // Hold the exec-gate READ across the landlock snapshot + fork so a
        // concurrent workspace carve-out (WRITE on the tagma) cannot interleave:
        // the snapshot below cannot be preceded by a carve that narrows the
        // writable set. No-op when no gate is configured.
        let _bg_gate = super::gate::ExecGate::read(&self.exec_gate).await;
        // Landlock-restrict the background bash to the agent's access decision
        // (Linux + landlock). Compose the decision (lock-manager-backed snapshot
        // + this task's own tmpdir as scratch) via `AccessSource`; `apply` is
        // pure mechanism — it moves the prepared landlock/mount-hole state into
        // the `pre_exec` closure held by `cmd` until `spawn()` consumes it.
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        if let Some(source) = &self.access_source {
            crate::landlock::apply(&mut cmd, &source.access_with_scratch(tmpdir.path())?)?;
        }
        let child = cmd.spawn()?;
        let pid = child.id();
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
                cancel,
                handle: Some(handle),
                tmpdir: Some(tmpdir),
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
    pub(super) async fn adopt(
        &mut self,
        child: Child,
        marker_read: Option<OwnedFd>,
        pumps: Pumps,
        output: TaskOutput,
        epoch_at_fork: Option<u64>,
    ) -> AdoptOutcome {
        let gate = self.exec_gate.clone();
        let _gate = super::gate::ExecGate::read(&gate).await;
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
                cancel,
                handle: Some(handle),
                tmpdir: None,
            },
        );
        AdoptOutcome::Adopted(id)
    }

    /// Read a background task's accumulated output and status.
    pub(super) fn read(&self, id: &str, tail_bytes: usize) -> Result<BgReadOutput, ShellError> {
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
        let code = task.exit_code.load(Ordering::Relaxed);
        Ok(BgReadOutput {
            output,
            state: TaskState::from_u8(task.state.load(Ordering::Relaxed)),
            exit_code: (code != EXIT_NONE).then_some(code),
            stalled: task.stalled.load(Ordering::Relaxed),
            bytes,
        })
    }

    /// Cancel and reap a background task. Its `TempDir` (holding `out.log`) is
    /// removed when the removed task drops, after the watcher has finished.
    pub(super) async fn kill(&mut self, id: &str) -> Result<(), ShellError> {
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
        // `task` drops here: its `TempDir` removes the output dir. The watcher
        // has already finished (we awaited the handle above), so `out.log` is
        // not read after removal.
        Ok(())
    }
}

impl Drop for BackgroundRegistry {
    fn drop(&mut self) {
        // For each task: force-kill the whole process group (sync — `Drop`
        // can't await the watcher); the task's `TempDir` removes its output dir
        // when drained. `kill_on_drop` alone would only signal the leader,
        // orphaning its children. The orphaned watcher's `out.log` reads are
        // already fallible, so a dir removed mid-poll is tolerated (same as the
        // old manual cleanup).
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
struct BgIncGuard(Option<Arc<super::gate::ExecGate>>);

impl BgIncGuard {
    fn new(gate: Arc<super::gate::ExecGate>) -> Self {
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
fn mark_terminal(state: &AtomicU8, terminal: u8, gate: Option<&Arc<super::gate::ExecGate>>) {
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

/// Watcher loop: detect exit, run the size + stall watchdogs, and drive a
/// two-phase kill on cancel. Branches per output source ([`TaskOutput`]):
/// spawned tasks stat their log file, adopted tasks total their live
/// captures — the watchdog semantics are identical either way.
async fn watch(args: WatchArgs) {
    let WatchArgs {
        mut child,
        watched,
        cancel,
        max_bg_bytes,
        id,
        on_terminal,
        exec_gate,
        output,
        mut pumps,
        marker_read,
    } = args;
    // Hold the adopted task's marker read end for this watcher's whole
    // life (see WatchArgs::marker_read); every `return` below drops it,
    // always after the child is terminal.
    let _marker_keepalive = marker_read;
    let Watched {
        state,
        exit_code,
        stalled,
        bytes,
    } = watched;
    let mut last_size: usize = 0;
    let mut quiescent_since: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = pgroup::kill_tree(&mut child).await;
                drain_pumps(&mut pumps).await;
                mark_terminal(&state, STATE_KILLED, exec_gate.as_ref());
                fire_terminal(&on_terminal, &id, TaskState::Killed, None);
                return;
            }
            _ = tokio::time::sleep(WATCH_POLL) => {}
        }

        let size = match &output {
            TaskOutput::File { path } => std::fs::metadata(path)
                .map(|m| m.len())
                .unwrap_or(0) as usize,
            TaskOutput::Pipes { out, err, .. } => pipes_total(out, err.as_ref()),
        };
        bytes.store(size, Ordering::Relaxed);


        // Size watchdog: unbounded output fills the disk (the 768GB lesson).
        if size > max_bg_bytes {
            let _ = pgroup::kill_tree(&mut child).await;
            drain_pumps(&mut pumps).await;
            mark_terminal(&state, STATE_KILLED, exec_gate.as_ref());
            fire_terminal(&on_terminal, &id, TaskState::Killed, None);
            return;
        }

        // Exit detection.
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                exit_code.store(code.unwrap_or(EXIT_NONE), Ordering::Relaxed);
                // Drain the adopted task's pumps (if any) WITHOUT consuming
                // the captures — post-exit reads must still see every byte.
                drain_pumps(&mut pumps).await;
                mark_terminal(&state, STATE_EXITED, exec_gate.as_ref());
                fire_terminal(&on_terminal, &id, TaskState::Exited, code);
                return;
            }
            Ok(None) => {}
            Err(_) => {
                drain_pumps(&mut pumps).await;
                mark_terminal(&state, STATE_EXITED, exec_gate.as_ref());
                fire_terminal(&on_terminal, &id, TaskState::Exited, None);
                return;
            }
        }

        // Stall watchdog: requires quiescence, then a tail-regex match (R6).
        if size == last_size {
            let since = quiescent_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= STALL_QUIESCENCE && tail_matches(&output) {
                stalled.store(true, Ordering::Relaxed);
            }
        } else {
            quiescent_since = None;
            stalled.store(false, Ordering::Relaxed);
        }
        last_size = size;
    }
}

/// Drain an adopted task's pipe pumps at terminal state, WITHOUT consuming
/// the captures (post-exit reads must still see the full output — taking
/// and finishing a capture here would empty the task). Mirrors the
/// foreground path: once the child dies the pipes close and the pumps see
/// EOF promptly; the bound only binds when a grandchild the command
/// backgrounded still holds a write-end.
///
/// Kill-path time budget: kill_tree (<=2s SIGTERM grace + SIGKILL reap)
/// plus at most 2x1s here must stay under KILL_JOIN's 5s, so a `kill()`
/// caller never waits past its own join bound. Keep these constants in
/// sync if any is retuned.
async fn drain_pumps(pumps: &mut Option<Pumps>) {
    let Some(p) = pumps.take() else { return };
    for mut handle in [Some(p.out), p.err].into_iter().flatten() {
        if tokio::time::timeout(super::backend::PUMP_DRAIN_DEADLINE, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await; // resolves promptly with Cancelled after abort
        }
    }
}

/// Total bytes seen across an adopted task's captured streams. Monotonic
/// even after clipping freezes the rendered view (the head freezes and the
/// tail rolls while `total` keeps counting), so both watchdogs and the
/// `bytes` counter keep moving while a clipped command keeps printing.
fn pipes_total(
    out: &Arc<Mutex<BoundedCapture>>,
    err: Option<&Arc<Mutex<BoundedCapture>>>,
) -> usize {
    lock_cap(out).total_bytes() + err.map(|e| lock_cap(e).total_bytes()).unwrap_or(0)
}

/// Lock a shared capture, tolerating a poisoned lock (a pump that panicked
/// mid-`push`). The inner capture is still perfectly readable, and the
/// poison must not take the watcher down before `mark_terminal` — that
/// would leak the running-bg tally and permanently refuse this agent's
/// carves.
fn lock_cap(cap: &Arc<Mutex<BoundedCapture>>) -> MutexGuard<'_, BoundedCapture> {
    cap.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stall tail probe per output source: the file's last [`STALL_TAIL`]
/// bytes, or an adopted task's rolling capture tail clipped to the same
/// window (the head and marker are deliberately not examined — only the
/// most recent output says what the command is waiting on).
fn tail_matches(output: &TaskOutput) -> bool {
    match output {
        TaskOutput::File { path } => tail_matches_prompt(path),
        TaskOutput::Pipes { out, err, mode } => {
            // Separate prompts may land on either stream, so probe both
            // tails (the same streams read_pipes_tail joins for reads).
            let tails: Vec<String> = match mode {
                CaptureMode::Merged | CaptureMode::Stdout => vec![lock_cap(out).tail_text()],
                CaptureMode::Stderr => vec![
                    lock_cap(err.as_ref().expect("stderr capture exists under Stderr")).tail_text()
                ],
                CaptureMode::Separate => {
                    let e = err.as_ref().expect("stderr capture exists under Separate");
                    vec![lock_cap(out).tail_text(), lock_cap(e).tail_text()]
                }
            };
            tails
                .iter()
                .any(|t| stall_regex().is_match(&clip_tail(t.clone(), STALL_TAIL as usize)))
        }
    }
}

/// Invoke the terminal-state observer, if registered. A panic in the callback
/// propagates through the watcher task (surfaced by tokio's default handler).
fn fire_terminal(
    on_terminal: &Option<OnTaskTerminal>,
    id: &str,
    state: TaskState,
    exit_code: Option<i32>,
) {
    if let Some(cb) = on_terminal {
        cb(id, state, exit_code);
    }
}

/// Interactive-prompt lockup patterns (kept conservative to avoid false positives).
fn stall_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)(password|passphrase|are you sure|confirm)").expect("valid regex")
    })
}

fn tail_matches_prompt(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(STALL_TAIL)));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    stall_regex().is_match(&String::from_utf8_lossy(&buf))
}

fn read_tail(path: &Path, tail_bytes: usize) -> Result<String, ShellError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(tail_bytes as u64)));
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Render an adopted task's captured output per `mode` (a live peek — the
/// streams may still be flowing) and clip to the last `tail_bytes`, the
/// same tail contract the file path honors. `Separate` joins the two
/// streams around a `[stderr]` divider: they were separate OS pipes all
/// along, so the true interleave never existed and this is a faithful
/// join, not a reconstruction.
fn read_pipes_tail(
    out: &Arc<Mutex<BoundedCapture>>,
    err: Option<&Arc<Mutex<BoundedCapture>>>,
    mode: CaptureMode,
    tail_bytes: usize,
) -> String {
    let peek = |cap: &Arc<Mutex<BoundedCapture>>, stream: &str| {
        super::backend::with_banner(stream, &lock_cap(cap).peek())
    };
    let rendered = match mode {
        CaptureMode::Merged => peek(out, "output"),
        CaptureMode::Stdout => peek(out, "stdout"),
        CaptureMode::Stderr => {
            peek(err.expect("stderr capture exists under Stderr"), "stderr")
        }
        CaptureMode::Separate => {
            let o = peek(out, "stdout");
            let e = peek(err.expect("stderr capture exists under Separate"), "stderr");
            if e.trim().is_empty() {
                o
            } else {
                format!("{o}\n[stderr]\n{e}")
            }
        }
    };
    clip_tail(rendered, tail_bytes)
}

/// Keep the last `tail_bytes` bytes of `s`, widening the cut to a char
/// boundary so no codepoint is sliced in half — the String twin of
/// `read_tail`'s byte-window clip.
fn clip_tail(mut s: String, tail_bytes: usize) -> String {
    if s.len() <= tail_bytes {
        return s;
    }
    let mut start = s.len() - tail_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.split_off(start)
}

/// Unlink an adopted task's live spill files once nothing will reference
/// them again (the task is leaving the registry — no later banner can name
/// the files, and nothing else reads them).
fn remove_pipes_spill(output: &TaskOutput) {
    if let TaskOutput::Pipes { out, err, .. } = output {
        for cap in std::iter::once(out).chain(err.iter()) {
            if let Some(path) = lock_cap(cap).spill_path() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Recorded terminal event `(task_id, state, exit_code)` for the on_terminal tests.
    type CapturedTerminal = Option<(String, TaskState, Option<i32>)>;

    fn registry() -> BackgroundRegistry {
        BackgroundRegistry::new(
            OsString::from("bash"),
            10 * 1024 * 1024,
            HashMap::new(),
            None,
        )
    }

    /// The headline carve-exclusion test, exercised through the real
    /// `BackgroundRegistry` (no landlock needed): a running background task
    /// bumps `running_bg` and makes the gate's WRITE side refuse a carve; once
    /// the task is killed and the watcher decs, the gate is writable again.
    /// This is the only test that drives the READ-across-fork + inc/dec wiring
    /// end-to-end, covering both the carve-exclusion claim and the inc_bg
    /// ordering (inc happens-before the watcher's dec).
    #[tokio::test]
    async fn running_bg_blocks_carve_while_task_runs() {
        let gate = crate::gate::ExecGate::new();
        let mut reg = registry().with_exec_gate(gate.clone());
        let id = reg.spawn("sleep 30").await.unwrap();

        // The spawn incs the tally before returning; wait for it to land.
        for _ in 0..50 {
            if gate.running_bg() == 1 {
                break;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        assert_eq!(gate.running_bg(), 1, "running task should bump the tally");
        assert_eq!(
            gate.try_write().unwrap_err(),
            crate::gate::ExecGateFailure::BgTasksRunning(1),
            "a carve must be refused while a background task runs"
        );

        reg.kill(&id).await.unwrap();
        // The watcher's cancel branch decs the tally; wait for it to settle.
        for _ in 0..50 {
            if gate.running_bg() == 0 {
                break;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        assert_eq!(gate.running_bg(), 0, "tally returns to zero after kill");
        assert!(
            gate.try_write().is_ok(),
            "a carve must succeed once no background task runs"
        );
    }

    #[tokio::test]
    async fn spawn_then_read_exited_task() {
        let mut reg = registry();
        let id = reg.spawn("echo hello").await.unwrap();
        // Wait for the task to exit and the watcher to notice.
        for _ in 0..50 {
            let out = reg.read(&id, 4096).unwrap();
            if out.state == TaskState::Exited {
                assert!(out.output.contains("hello"));
                assert_eq!(out.exit_code, Some(0));
                return;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        panic!("task did not exit in time");
    }

    /// Color-suppression env vars reach a background task too (the `COLOR_VARS`
    /// const is shared with the foreground path): all four entries are applied
    /// — `TERM`/`NO_COLOR`/`CLICOLOR` set, `LS_COLORS` emptied.
    #[tokio::test]
    async fn spawn_applies_color_vars() {
        let mut reg = registry();
        let id = reg
            .spawn("echo \"$TERM/$NO_COLOR/$CLICOLOR\"; test -z \"$LS_COLORS\" && echo empty")
            .await
            .unwrap();
        for _ in 0..50 {
            let out = reg.read(&id, 4096).unwrap();
            if out.state == TaskState::Exited {
                assert_eq!(out.exit_code, Some(0));
                assert_eq!(out.output.trim(), "dumb/1/0\nempty");
                return;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        panic!("task did not exit in time");
    }

    #[tokio::test]
    async fn kill_stops_a_long_task() {
        let mut reg = registry();
        let id = reg.spawn("sleep 30").await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        reg.kill(&id).await.unwrap();
        let err = reg.read(&id, 4096).unwrap_err();
        assert!(matches!(err, ShellError::TaskNotFound { .. }));
    }

    #[tokio::test]
    async fn read_unknown_task_errors() {
        let reg = registry();
        let err = reg.read("nope", 4096).unwrap_err();
        assert!(matches!(err, ShellError::TaskNotFound { .. }));
    }

    #[tokio::test]
    async fn size_watchdog_kills_overflow() {
        let mut reg = BackgroundRegistry::new(
            OsString::from("bash"),
            4096, // tiny cap
            HashMap::new(),
            None,
        );
        let id = reg.spawn("yes hello").await.unwrap();
        for _ in 0..100 {
            let out = reg.read(&id, 1024).unwrap();
            if out.state == TaskState::Killed {
                return;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        panic!("size watchdog did not kill the overflowing task");
    }

    /// Build a registry whose terminal-state observer records the last
    /// `(task_id, state, exit_code)` into the returned shared slot.
    fn tracking_registry(
        max_bg_bytes: usize,
    ) -> (BackgroundRegistry, Arc<std::sync::Mutex<CapturedTerminal>>) {
        use std::sync::Mutex;
        let received: Arc<Mutex<CapturedTerminal>> = Arc::new(Mutex::new(None));
        let captured = received.clone();
        let reg = BackgroundRegistry::new(
            OsString::from("bash"),
            max_bg_bytes,
            HashMap::new(),
            Some(Arc::new(move |id, state, code| {
                *captured.lock().unwrap() = Some((id.to_string(), state, code));
            })),
        );
        (reg, received)
    }

    #[tokio::test]
    async fn on_terminal_fires_on_exit() {
        let (mut reg, received) = tracking_registry(10 * 1024 * 1024);
        let id = reg.spawn("exit 7").await.unwrap();
        for _ in 0..50 {
            if received.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        let (cb_id, cb_state, cb_code) = received
            .lock()
            .unwrap()
            .clone()
            .expect("on_terminal did not fire on exit");
        assert_eq!(cb_id, id);
        assert_eq!(cb_state, TaskState::Exited);
        assert_eq!(cb_code, Some(7));
    }

    #[tokio::test]
    async fn on_terminal_fires_on_size_watchdog() {
        // tiny cap → the size watchdog kills quickly.
        let (mut reg, received) = tracking_registry(4096);
        let id = reg.spawn("yes hello").await.unwrap();
        for _ in 0..100 {
            if received.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
        let (cb_id, cb_state, _cb_code) = received
            .lock()
            .unwrap()
            .clone()
            .expect("on_terminal did not fire on size-watchdog kill");
        assert_eq!(cb_id, id);
        assert_eq!(cb_state, TaskState::Killed);
    }
}
