//! Builder for [`ProcessBackend`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::ShellError;
use crate::stateless::{
    backend::ProcessBackend,
    env_snapshot::EnvSnapshot,
    supervisor::{self, TaskState, TerminalObserver},
};

const DEFAULT_FALLBACK_CWD: &str = "/tmp";
const DEFAULT_FALLBACK_SHELL: &str = "/bin/bash";
/// In-memory tail retained per stream (stdout/stderr) before clipping.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MiB
/// Output cap for a background task before the size watchdog kills it.
const DEFAULT_MAX_BG_BYTES: usize = 100 * 1024 * 1024; // 100 MiB

/// Builder for [`ProcessBackend`].
///
/// Construct with [`StatelessBuilder::new`], chain setters to override defaults,
/// then [`build`](Self::build) to capture the env snapshot and create the
/// backend.
///
/// # Defaults
///
/// | Field              | Default     | Effect                                                        |
/// |--------------------|-------------|---------------------------------------------------------------|
/// | `shell`            | `"bash"`    | Program spawned per call                                      |
/// | `fallback_cwd`     | `"/tmp"`   | cwd when `current_dir()` fails or a cached cwd was deleted     |
/// | `fallback_shell`   | `/bin/bash`| `$SHELL` for env capture when the env var is unset             |
/// | `max_output_bytes` | 1 MiB       | Per-stream in-memory tail before output is clipped             |
/// | `max_bg_bytes`     | 100 MiB     | Background-task output cap before the size watchdog kills it   |
/// | `data_dir`         | resolved    | Root for the env snapshot, per-call wrappers, bg output        |
#[derive(Clone, Debug)]
pub struct StatelessBuilder {
    pub(super) shell: OsString,
    pub(super) fallback_cwd: PathBuf,
    pub(super) fallback_shell: String,
    pub(super) initial_cwd: Option<PathBuf>,
    pub(super) env: HashMap<OsString, OsString>,
    pub(super) max_output_bytes: usize,
    pub(super) max_bg_bytes: usize,
    pub(super) data_dir: Option<PathBuf>,
    pub(super) on_terminal: Option<TerminalObserver>,
}

impl StatelessBuilder {
    /// Creates a builder with default settings.
    pub fn new() -> Self {
        Self {
            shell: OsString::from("bash"),
            fallback_cwd: PathBuf::from(DEFAULT_FALLBACK_CWD),
            fallback_shell: DEFAULT_FALLBACK_SHELL.to_owned(),
            initial_cwd: None,
            env: HashMap::new(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_bg_bytes: DEFAULT_MAX_BG_BYTES,
            data_dir: None,
            on_terminal: None,
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

    /// Overrides the fallback `$SHELL` used for env capture. Default: `/bin/bash`.
    pub fn fallback_shell(mut self, shell: impl Into<String>) -> Self {
        self.fallback_shell = shell.into();
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

    /// Overrides the per-backend data directory (default: resolved from
    /// `$JUST_AGENT_DATA_DIR` or the platform data dir).
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
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

    /// Resolve the per-backend data dir: explicit override, else
    /// `$JUST_AGENT_DATA_DIR`, else the platform data dir.
    fn resolve_data_dir(&self) -> Result<PathBuf, ShellError> {
        if let Some(dir) = &self.data_dir {
            return Ok(dir.clone());
        }
        let base = match std::env::var("JUST_AGENT_DATA_DIR") {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => dirs::data_dir().ok_or_else(|| {
                ShellError::backend("could not determine platform data directory")
            })?,
        };
        Ok(base.join("just-agent").join("stateless"))
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

    /// Captures the env snapshot and constructs a [`ProcessBackend`].
    pub async fn build(self) -> Result<ProcessBackend, ShellError> {
        self.validate()?;
        let data_dir = self.resolve_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;

        let initial_cwd = match &self.initial_cwd {
            Some(cwd) => cwd.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| self.fallback_cwd.clone()),
        };

        // Capture the user's shell env once; replayed per call by the wrapper.
        let shell = if self.shell.is_empty() {
            OsString::from(&self.fallback_shell)
        } else {
            self.shell.clone()
        };
        let env_snapshot = EnvSnapshot::capture(&data_dir, shell.clone())?;

        let background = supervisor::BackgroundRegistry::new(
            shell,
            env_snapshot.path.clone(),
            data_dir.clone(),
            self.max_bg_bytes,
            self.env.clone(),
            self.on_terminal.clone().map(|o| o.0),
        );

        Ok(ProcessBackend {
            config: self,
            cwd: initial_cwd,
            data_dir,
            env_snapshot,
            next_call: 0,
            background,
        })
    }
}

impl Default for StatelessBuilder {
    fn default() -> Self {
        Self::new()
    }
}
