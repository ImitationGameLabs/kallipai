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

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

mod registry;
mod tail;
#[cfg(all(test, unix))]
mod tests;
mod watch;

pub(super) use registry::{AdoptOutcome, BackgroundRegistry, Pumps, TaskOutput};

/// LLM-facing identifier for a background task (UUID v4 string).
pub(super) type TaskId = String;

const WATCH_POLL: Duration = Duration::from_millis(200);
/// Stall requires this much output-quiescence before the tail regex is trusted,
/// so a build log printing `Compiling foo:` can't trip it.
const STALL_QUIESCENCE: Duration = Duration::from_secs(3);
/// Tail size examined for interactive-prompt lockups.
const STALL_TAIL: u64 = 4 * 1024;
/// Bounded wait for a watcher task to finish after cancel.
const KILL_JOIN: Duration = Duration::from_secs(5);

/// Snapshot cap for the per-agent background-task view: a defensive bound
/// (the concurrent in-flight count does not realistically reach it),
/// applied at the source so the published list is bounded by construction.
/// It must stay equal to the tagma protocol's `BACKGROUND_TASK_LIST_MAX`,
/// which caps the same list on the wire: a larger shell cap would let the
/// tagma drop entries without saying so. `kallipai-adk` asserts the two
/// equal at compile time.
pub const MAX_BG_SUMMARIES: usize = 20;

const EXIT_NONE: i32 = -1;
const STATE_RUNNING: u8 = 0;
const STATE_EXITED: u8 = 1;
const STATE_KILLED: u8 = 2;

/// Visible task state (serialized for the LLM as a lowercase string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
/// `(task_id, state, exit_code, reason)`; `exit_code` is `None` for killed /
/// watcher-error cases, and `reason` carries the termination story when there
/// is one (disk cap, poison degrade) — `None` for a plain exit or a watchdog
/// kill, whose story the `Killed` state already tells. Best-effort: may not
/// fire on registry `Drop` — the runtime may be shutting down and the watcher
/// cannot be awaited synchronously, so callers must tolerate a missed
/// notification (equivalent to the task being reclaimed).
pub type OnTaskTerminal = Arc<dyn Fn(&str, TaskState, Option<i32>, Option<&str>) + Send + Sync>;

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
    /// Termination story when there is one (disk cap, poison degrade) —
    /// the same text that prefixes the read body, exposed structurally so
    /// field-level consumers need not parse the output text.
    pub reason: Option<String>,
}

/// One in-flight background task, as read from the published cell.
///
/// A read-only projection: counters as of the call, no output. The command text
/// is already collapsed to a single line and capped in length by the registry,
/// so consumers print it verbatim; `started_at` is epoch seconds, and rendering
/// an age from it is the display side's business.
#[derive(Debug, Clone)]
pub struct BgTaskSummary {
    /// Task id (UUID v4 string), the same one handed to the LLM.
    pub id: String,
    /// `true` if the stall watchdog flagged an interactive-prompt lockup.
    pub stalled: bool,
    /// Total bytes of output written so far.
    pub bytes: usize,
    /// The launched command, single-lined and capped (see the type docs).
    pub command: String,
    /// The directory the task started in, recorded at spawn; a launch-time snapshot.
    pub cwd: PathBuf,
    /// Spawn time, epoch seconds.
    pub started_at: u64,
}

/// One published entry: the fixed launch facts of a running task, plus the live
/// counters its watcher updates.
///
/// The counters are the watcher's own atomics, so a read reports the task's
/// current output size and stall flag rather than the values it had when the
/// running set last changed. `id`, `command`, `cwd`, and `started_at` never
/// change after the entry is published.
#[derive(Debug, Clone)]
pub struct BgTaskHandle {
    /// Task id (UUID v4 string), the same one handed to the LLM.
    pub id: String,
    /// Stall flag, shared with the task's watcher.
    pub stalled: Arc<AtomicBool>,
    /// Captured-output byte count, shared with the task's watcher.
    pub bytes: Arc<AtomicUsize>,
    /// The launched command, single-lined and capped (see [`BgTaskSummary`]).
    pub command: String,
    /// The directory the task started in, recorded at spawn; a launch-time snapshot.
    pub cwd: PathBuf,
    /// Spawn time, epoch seconds.
    pub started_at: u64,
}

impl BgTaskHandle {
    /// Read the entry as the status projection, counters as of this call.
    pub fn summary(&self) -> BgTaskSummary {
        BgTaskSummary {
            id: self.id.clone(),
            stalled: self.stalled.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            started_at: self.started_at,
        }
    }
}

/// The shared cell a host installs to receive the running background tasks.
///
/// The registry replaces its contents whenever the running set changes and
/// empties it when the registry goes away, so a reader never sees a finished
/// task. A `std` mutex on purpose: reading it must not queue behind the
/// backend's async lock, which a long foreground command holds throughout. Each
/// entry carries live counters (see [`BgTaskHandle`]), so a read of the cell is
/// never a stale frame.
pub type BgTaskSink = Arc<std::sync::Mutex<Vec<BgTaskHandle>>>;
