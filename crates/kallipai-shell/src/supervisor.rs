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

use std::sync::Arc;
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
