//! Builder for [`ProcessBackend`].

use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(all(target_os = "linux", feature = "landlock"))]
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::ProcessBackend;
use crate::error::ShellError;
use crate::supervisor::{self, TaskState, TerminalObserver};

const DEFAULT_FALLBACK_CWD: &str = "/tmp";
/// In-memory tail retained per stream (stdout/stderr) before clipping.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MiB
/// Output cap for a background task before the size watchdog kills it.
const DEFAULT_MAX_BG_BYTES: usize = 100 * 1024 * 1024; // 100 MiB

/// The closure type inside [`AccessSource`]: a per-spawn snapshot of the
/// owning agent's [`AccessDecision`]. Aliased so the newtype signature stays on
/// one line.
#[cfg(all(target_os = "linux", feature = "landlock"))]
pub(crate) type AccessSourceFn =
    Arc<dyn Fn() -> io::Result<crate::landlock::AccessDecision> + Send + Sync + 'static>;

/// Owned snapshot source of an agent's per-spawn [`AccessDecision`] (read
/// policy + writable set + readonly holes), used by landlock/mount-ns
/// enforcement (Linux + `landlock`). Wrapped in a newtype so the closure can
/// live in a `#[derive(Debug)]` builder. Agent-agnostic: the runtime composes
/// the decision from its permission class + the dirlock coordinator and hands it
/// to the shell here, keeping the shell decoupled from agent identity/tiers.
#[cfg(all(target_os = "linux", feature = "landlock"))]
#[derive(Clone)]
pub(crate) struct AccessSource(pub(crate) AccessSourceFn);

#[cfg(all(target_os = "linux", feature = "landlock"))]
impl std::fmt::Debug for AccessSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessSource").finish_non_exhaustive()
    }
}

#[cfg(all(target_os = "linux", feature = "landlock"))]
impl AccessSource {
    /// Snapshot the agent's access decision as-is, ready for
    /// [`crate::landlock::apply`]. Used by the foreground path, whose only
    /// on-disk writes are overflow spill files under `spill_dir`
    /// (`baseline_writable` in `apply` already covers `temp_dir()`, `/dev/null`,
    /// ...). The snapshot error propagates (via `?`) rather than silently
    /// producing an empty writable list, which would deny all of the agent's
    /// writes.
    pub(crate) fn access(&self) -> io::Result<crate::landlock::AccessDecision> {
        (self.0)()
    }

    /// Snapshot the agent's access decision and append `scratch` (a per-task
    /// writable dir) to its writable set, returning the decision ready for
    /// [`crate::landlock::apply`]. Used by the background path, whose `out.log`
    /// lives in a per-task tmpdir. The snapshot error propagates (via `?`).
    pub(crate) fn access_with_scratch(
        &self,
        scratch: &std::path::Path,
    ) -> io::Result<crate::landlock::AccessDecision> {
        let mut decision = (self.0)()?;
        decision.writable.push(scratch.to_path_buf());
        Ok(decision)
    }
}

/// Builder for [`ProcessBackend`].
///
/// Construct with [`ShellBuilder::new`], chain setters to override defaults,
/// then [`build`](Self::build) to create the backend.
///
/// # Defaults
///
/// | Field              | Default     | Effect                                                        |
/// |--------------------|-------------|---------------------------------------------------------------|
/// | `shell`            | `"bash"`    | Program spawned per call                                      |
/// | `fallback_cwd`     | `"/tmp"`   | cwd when `current_dir()` fails or a cached cwd was deleted     |
/// | `max_output_bytes` | 1 MiB       | Per-stream in-memory head+tail before output is clipped        |
/// | `max_bg_bytes`     | 100 MiB     | Background-task output cap before the size watchdog kills it   |
/// | `spill_dir`        | `$TMPDIR/kallip` | Where overflow spill files are written (overflow only)    |
#[derive(Clone, Debug)]
pub struct ShellBuilder {
    pub(super) shell: OsString,
    pub(super) fallback_cwd: PathBuf,
    pub(super) initial_cwd: Option<PathBuf>,
    pub(super) env: HashMap<OsString, OsString>,
    pub(super) max_output_bytes: usize,
    pub(super) max_bg_bytes: usize,
    /// Directory where overflow spill files are written (only when a captured
    /// stream exceeds `max_output_bytes`). Defaults to `temp_dir()/kallip`.
    /// Spill files persist until the system temp cleaner reaps them; cwd
    /// recovery is file-free (a private fd channel), so this holds only the
    /// spill files. Must match the landlocked child's notion of the temp dir
    /// (`baseline_writable` uses `std::env::temp_dir()` at spawn), so do not
    /// `env_clear` or override `TMPDIR` while leaving this default.
    pub(super) spill_dir: PathBuf,
    pub(super) on_terminal: Option<TerminalObserver>,
    /// Optional source of this backend's agent's per-spawn access decision
    /// (read policy + writable set + readonly holes). When set (and the
    /// `landlock` feature is on, Linux), every spawned `bash` is restricted by a
    /// landlock domain derived from the snapshot this closure returns. Kept
    /// generic so the shell crate has no dependency on the lock coordinator.
    /// `Result` is load-bearing: an error surfaces rather than silently denying
    /// all writes.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    pub(super) access_source: Option<AccessSource>,
}

impl ShellBuilder {
    /// Creates a builder with default settings.
    pub fn new() -> Self {
        Self {
            shell: OsString::from("bash"),
            fallback_cwd: PathBuf::from(DEFAULT_FALLBACK_CWD),
            initial_cwd: None,
            env: HashMap::new(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_bg_bytes: DEFAULT_MAX_BG_BYTES,
            spill_dir: std::env::temp_dir().join("kallip"),
            on_terminal: None,
            #[cfg(all(target_os = "linux", feature = "landlock"))]
            access_source: None,
        }
    }

    /// Overrides the shell program spawned per call. Default: `"bash"`.
    pub fn shell(mut self, shell: impl Into<OsString>) -> Self {
        self.shell = shell.into();
        self
    }

    /// Overrides the fallback working directory. Default: `"/tmp"`.
    pub fn fallback_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.fallback_cwd = cwd.into();
        self
    }

    /// Overrides the initial working directory (default: the process cwd).
    pub fn initial_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.initial_cwd = Some(cwd.into());
        self
    }

    /// Overrides the per-stream in-memory output tail (bytes). Default: 1 MiB.
    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Overrides the background-task output cap (bytes). Default: 100 MiB.
    pub fn max_bg_bytes(mut self, bytes: usize) -> Self {
        self.max_bg_bytes = bytes;
        self
    }

    /// Overrides the spill directory for overflow output files. Default:
    /// `temp_dir()/kallip`. Useful in tests to point at a `tempfile::TempDir`
    /// for hermetic cleanup.
    pub fn spill_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.spill_dir = dir.into();
        self
    }

    /// Registers an observer invoked when a background task reaches a terminal
    /// state (exited/killed). Called as `(task_id, state, exit_code)`; `exit_code`
    /// is `None` for killed / watcher-error cases. Best-effort: may not fire on
    /// registry `Drop` (the runtime may be shutting down and the watcher cannot
    /// be awaited synchronously).
    pub fn on_terminal<F>(mut self, cb: F) -> Self
    where
        F: Fn(&str, TaskState, Option<i32>) + Send + Sync + 'static,
    {
        self.on_terminal = Some(TerminalObserver(Arc::new(cb)));
        self
    }

    /// Adds an environment variable applied to every command.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Adds multiple environment variables applied to every command.
    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        for (k, v) in envs {
            self.env.insert(k.into(), v.into());
        }
        self
    }

    /// Sets the source of this backend's agent's per-spawn access decision
    /// (read policy + writable set + readonly holes), enabling landlock/mount-ns
    /// enforcement on every spawned `bash` (Linux + `landlock` feature only).
    /// The closure should return a point-in-time snapshot of the agent's
    /// composed access decision.
    #[cfg(all(target_os = "linux", feature = "landlock"))]
    pub fn access_source<F>(mut self, source: F) -> Self
    where
        F: Fn() -> io::Result<crate::landlock::AccessDecision> + Send + Sync + 'static,
    {
        self.access_source = Some(AccessSource(Arc::new(source)));
        self
    }

    /// Validates the configuration.
    pub fn validate(&self) -> Result<(), ShellError> {
        if self.shell.is_empty() {
            return Err(ShellError::backend("shell must not be empty"));
        }
        if self.max_output_bytes == 0 {
            return Err(ShellError::backend("max_output_bytes must be > 0"));
        }
        if self.max_bg_bytes == 0 {
            return Err(ShellError::backend("max_bg_bytes must be > 0"));
        }
        Ok(())
    }

    /// Constructs a [`ProcessBackend`].
    ///
    /// Foreground `exec` passes its script via `bash -c` argv and recovers cwd
    /// over a private fd channel (no per-call files); on output overflow it
    /// writes a spill file to `spill_dir` and nowhere else. Background tasks
    /// each own an auto-cleaned tmpdir. Nothing is resolved from or written
    /// under a data dir.
    pub async fn build(self) -> Result<ProcessBackend, ShellError> {
        self.validate()?;

        let initial_cwd = match &self.initial_cwd {
            Some(cwd) => cwd.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| self.fallback_cwd.clone()),
        };

        // Spill layout: `<spill_dir>/bash_exec-<nonce>-<stream>.txt` (spill_dir
        // defaults to `temp_dir()/kallip`). The per-exec nonce in the filename
        // makes collisions impossible, so no per-backend subdir is needed. The
        // dir is created lazily, only when an overflow actually spills (see
        // `capture.rs`), so an under-budget backend writes nothing to disk.
        // The load-bearing symlink rejection is the `O_NOFOLLOW` dir open at
        // overflow time (TOCTOU-safe: it pins the dir inode and refuses a
        // symlink planted at the path at any time). This build-time stat is only
        // an early, clear fail-closed error so a hostile symlink surfaces at
        // startup rather than at the first overflow; it is not the seal.
        let is_symlink =
            std::fs::symlink_metadata(&self.spill_dir).is_ok_and(|m| m.file_type().is_symlink());
        if is_symlink {
            return Err(ShellError::backend(format!(
                "spill_dir {} must not be a symlink",
                self.spill_dir.display()
            )));
        }

        let background = supervisor::BackgroundRegistry::new(
            self.shell.clone(),
            self.max_bg_bytes,
            self.env.clone(),
            self.on_terminal.clone().map(|o| o.0),
        );
        // Share the access-decision snapshot source with the background registry
        // (cloned before `self` moves into `ProcessBackend.config`).
        #[cfg(all(target_os = "linux", feature = "landlock"))]
        let background = match &self.access_source {
            Some(source) => background.with_access_source(source.clone()),
            None => background,
        };

        Ok(ProcessBackend {
            config: self,
            cwd: initial_cwd,
            background,
        })
    }
}

impl Default for ShellBuilder {
    fn default() -> Self {
        Self::new()
    }
}
