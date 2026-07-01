//! Background-process supervisor (Claude-Code style).
//!
//! Each background task is its own `bash` process writing merged stdout/stderr
//! to a file; a watcher task polls it to detect exit, run a stall watchdog
//! (quiescence + tail regex) and a size watchdog, and drive a two-phase kill
//! on cancel. Modeled on the daemon's agent registry (`state.rs`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::error::ShellError;
use crate::stateless::pgroup;

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
/// so it can live in a `#[derive(Debug)]` struct like `StatelessBuilder`.
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
    /// Tail of the merged stdout/stderr file.
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
    output_path: PathBuf,
    /// Process-group leader pid (PGID == pid); used to force-kill the whole
    /// group on registry drop, since `Drop` can't await the watcher.
    pid: Option<u32>,
    state: Arc<AtomicU8>,
    exit_code: Arc<AtomicI32>,
    stalled: Arc<AtomicBool>,
    bytes: Arc<AtomicUsize>,
    cancel: CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// Shared mutable state observed by both the watcher task and read/kill.
struct Watched {
    state: Arc<AtomicU8>,
    exit_code: Arc<AtomicI32>,
    stalled: Arc<AtomicBool>,
    bytes: Arc<AtomicUsize>,
}

/// Registry of background tasks by id.
pub(super) struct BackgroundRegistry {
    tasks: HashMap<TaskId, BackgroundTask>,
    shell: OsString,
    data_dir: PathBuf,
    max_bg_bytes: usize,
    env: HashMap<OsString, OsString>,
    on_terminal: Option<OnTaskTerminal>,
    /// When set (Linux + `landlock` feature), each background `bash` is
    /// landlock-restricted to the owning agent's current access decision.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    access_source: Option<super::builder::AccessSource>,
}

impl BackgroundRegistry {
    pub(super) fn new(
        shell: OsString,
        data_dir: PathBuf,
        max_bg_bytes: usize,
        env: HashMap<OsString, OsString>,
        on_terminal: Option<OnTaskTerminal>,
    ) -> Self {
        Self {
            tasks: HashMap::new(),
            shell,
            data_dir,
            max_bg_bytes,
            env,
            on_terminal,
            #[cfg(all(target_os = "linux", feature = "landlock"))]
            access_source: None,
        }
    }

    /// Enable landlock enforcement on background tasks using the given
    /// access-decision snapshot source (the owning agent's composed decision).
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    pub(super) fn with_access_source(mut self, source: super::builder::AccessSource) -> Self {
        self.access_source = Some(source);
        self
    }

    /// Spawn `command` as a background task; returns its id.
    pub(super) fn spawn(&mut self, command: &str) -> Result<TaskId, ShellError> {
        let id = uuid::Uuid::new_v4().to_string();
        let task_dir = self.data_dir.join("bg").join(&id);
        std::fs::create_dir_all(&task_dir)?;
        let output_path = task_dir.join("out.log");

        // Create the output file so the redirect target exists before spawn.
        let output = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output_path)?;

        let wrapper_path = task_dir.join("cmd.sh");
        // Background wrapper: no cwd EXIT trap (background must not touch the
        // shared sticky cwd — M9).
        let wrapper = super::backend::build_wrapper(command, None);
        std::fs::write(&wrapper_path, wrapper)?;

        let mut cmd = Command::new(&self.shell);
        cmd.arg(&wrapper_path)
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
        // Landlock-restrict the background bash to the agent's access decision
        // (Linux + landlock). Compose the decision (lock-manager-backed snapshot
        // + this registry's data dir) via `AccessSource`; `apply` is pure
        // mechanism — it moves the prepared landlock/mount-hole state into the
        // `pre_exec` closure held by `cmd` until `spawn()` consumes it.
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        if let Some(source) = &self.access_source {
            crate::landlock::apply(&mut cmd, &source.access_with_scratch(&self.data_dir)?)?;
        }
        let child = cmd.spawn()?;
        let pid = child.id();

        let state = Arc::new(AtomicU8::new(STATE_RUNNING));
        let exit_code = Arc::new(AtomicI32::new(EXIT_NONE));
        let stalled = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(watch(
            child,
            output_path.clone(),
            Watched {
                state: state.clone(),
                exit_code: exit_code.clone(),
                stalled: stalled.clone(),
                bytes: bytes.clone(),
            },
            cancel.clone(),
            self.max_bg_bytes,
            id.clone(),
            self.on_terminal.clone(),
        ));

        self.tasks.insert(
            id.clone(),
            BackgroundTask {
                output_path,
                pid,
                state,
                exit_code,
                stalled,
                bytes,
                cancel,
                handle: Some(handle),
            },
        );
        Ok(id)
    }

    /// Read a background task's accumulated output and status.
    pub(super) fn read(&self, id: &str, tail_bytes: usize) -> Result<BgReadOutput, ShellError> {
        let task = self
            .tasks
            .get(id)
            .ok_or_else(|| ShellError::task_not_found(id))?;
        let bytes = task.bytes.load(Ordering::Relaxed);
        let output = read_tail(&task.output_path, tail_bytes)?;
        let code = task.exit_code.load(Ordering::Relaxed);
        Ok(BgReadOutput {
            output,
            state: TaskState::from_u8(task.state.load(Ordering::Relaxed)),
            exit_code: (code != EXIT_NONE).then_some(code),
            stalled: task.stalled.load(Ordering::Relaxed),
            bytes,
        })
    }

    /// Cancel and reap a background task, then remove its on-disk output dir.
    pub(super) async fn kill(&mut self, id: &str) -> Result<(), ShellError> {
        let mut task = self
            .tasks
            .remove(id)
            .ok_or_else(|| ShellError::task_not_found(id))?;
        task.cancel.cancel();
        if let Some(handle) = task.handle.take() {
            let _ = tokio::time::timeout(KILL_JOIN, handle).await;
        }
        // The agent killed it explicitly — drop the output dir.
        if let Some(dir) = task.output_path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }
}

impl Drop for BackgroundRegistry {
    fn drop(&mut self) {
        // For each task: force-kill the whole process group (sync — `Drop`
        // can't await the watcher) and remove its output dir. `kill_on_drop`
        // alone would only signal the leader, orphaning its children.
        for (_, task) in self.tasks.drain() {
            task.cancel.cancel();
            if let Some(pid) = task.pid {
                pgroup::force_kill_group(pid as i32);
            }
            if let Some(dir) = task.output_path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// Watcher loop: detect exit, run the size + stall watchdogs, and drive a
/// two-phase kill on cancel.
async fn watch(
    mut child: Child,
    output_path: PathBuf,
    watched: Watched,
    cancel: CancellationToken,
    max_bg_bytes: usize,
    id: String,
    on_terminal: Option<OnTaskTerminal>,
) {
    let Watched {
        state,
        exit_code,
        stalled,
        bytes,
    } = watched;
    let mut last_size: u64 = 0;
    let mut quiescent_since: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = pgroup::kill_tree(&mut child).await;
                state.store(STATE_KILLED, Ordering::Relaxed);
                fire_terminal(&on_terminal, &id, TaskState::Killed, None);
                return;
            }
            _ = tokio::time::sleep(WATCH_POLL) => {}
        }

        let size = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        bytes.store(size as usize, Ordering::Relaxed);

        // Size watchdog: unbounded output fills the disk (the 768GB lesson).
        if (size as usize) > max_bg_bytes {
            let _ = pgroup::kill_tree(&mut child).await;
            state.store(STATE_KILLED, Ordering::Relaxed);
            fire_terminal(&on_terminal, &id, TaskState::Killed, None);
            return;
        }

        // Exit detection.
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                exit_code.store(code.unwrap_or(EXIT_NONE), Ordering::Relaxed);
                state.store(STATE_EXITED, Ordering::Relaxed);
                fire_terminal(&on_terminal, &id, TaskState::Exited, code);
                return;
            }
            Ok(None) => {}
            Err(_) => {
                state.store(STATE_EXITED, Ordering::Relaxed);
                fire_terminal(&on_terminal, &id, TaskState::Exited, None);
                return;
            }
        }

        // Stall watchdog: requires quiescence, then a tail-regex match (R6).
        if size == last_size {
            let since = quiescent_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= STALL_QUIESCENCE && tail_matches_prompt(&output_path) {
                stalled.store(true, Ordering::Relaxed);
            }
        } else {
            quiescent_since = None;
            stalled.store(false, Ordering::Relaxed);
        }
        last_size = size;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Recorded terminal event `(task_id, state, exit_code)` for the on_terminal tests.
    type CapturedTerminal = Option<(String, TaskState, Option<i32>)>;

    fn registry() -> BackgroundRegistry {
        let dir = std::env::temp_dir().join(format!(
            "ja-sup-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        BackgroundRegistry::new(
            OsString::from("bash"),
            dir,
            10 * 1024 * 1024,
            HashMap::new(),
            None,
        )
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn spawn_then_read_exited_task() {
        let mut reg = registry();
        let id = reg.spawn("echo hello").unwrap();
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
        let id = reg.spawn("sleep 30").unwrap();
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
        let mut reg = {
            let dir = std::env::temp_dir().join(format!(
                "ja-sup-size-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            BackgroundRegistry::new(OsString::from("bash"), dir, 4096, HashMap::new(), None) // tiny cap
        };
        let id = reg.spawn("yes hello").unwrap();
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
        label: &str,
        max_bg_bytes: usize,
    ) -> (BackgroundRegistry, Arc<std::sync::Mutex<CapturedTerminal>>) {
        use std::sync::Mutex;
        let received: Arc<Mutex<CapturedTerminal>> = Arc::new(Mutex::new(None));
        let captured = received.clone();
        let dir = std::env::temp_dir().join(format!(
            "ja-sup-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = BackgroundRegistry::new(
            OsString::from("bash"),
            dir,
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
        let (mut reg, received) = tracking_registry("cb", 10 * 1024 * 1024);
        let id = reg.spawn("exit 7").unwrap();
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
        let (mut reg, received) = tracking_registry("cbsize", 4096);
        let id = reg.spawn("yes hello").unwrap();
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
