//! Builder for [`PtyBackend`](super::PtyBackend).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use super::PtyBackend;
use crate::error::ShellError;

// Default values matching previous hardcoded constants.
pub(super) const DEFAULT_ROWS: u16 = 24;
pub(super) const DEFAULT_COLS: u16 = 500;
pub(super) const DEFAULT_SCROLLBACK_LINES: usize = 10_000;
pub(super) const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const DEFAULT_STABILITY_THRESHOLD: usize = 3;
pub(super) const DEFAULT_FALLBACK_CWD: &str = "/tmp";
pub(super) const DEFAULT_FALLBACK_SHELL: &str = "/bin/bash";

/// Builder for [`PtyBackend`](super::PtyBackend).
///
/// Construct with [`PtyBuilder::new`], chain setter methods to override
/// defaults, then call [`build`](PtyBuilder::build) to create the backend.
///
/// # Defaults
///
/// | Field                 | Default        | Effect                                                              |
/// |-----------------------|----------------|---------------------------------------------------------------------|
/// | `argv`                | `["bash"]`    | Program (and args) spawned inside the PTY                           |
/// | `login_shell`         | `true`         | Appends `--login` (or `--noprofile --norc` in clean-env) to argv    |
/// | `rows`                | `24`           | PTY terminal height in rows                                         |
/// | `cols`                | `500`          | PTY terminal width in columns — wide to avoid line wrapping         |
/// | `scrollback_lines`    | `10_000`       | Max lines retained per session in memory                            |
/// | `poll_interval`       | `100 ms`       | Sleep between output-polling reads; lower = faster, more CPU        |
/// | `stability_threshold` | `3`            | Consecutive identical reads before output is considered stable       |
/// | `fallback_cwd`        | `"/tmp"`      | Working dir when caller provides none and `current_dir()` fails     |
/// | `fallback_shell`      | `"/bin/bash"` | `$SHELL` in clean-env mode when the env var is unset                |
///
/// # Example
///
/// ```ignore
/// use std::time::Duration;
/// use just_agent_shell::PtyBuilder;
///
/// let backend = PtyBuilder::new("main")
///     .dimensions(40, 1000)
///     .scrollback_lines(50_000)
///     .poll_interval(Duration::from_millis(50))
///     .build()
///     .await?;
/// ```
#[derive(Clone, Debug)]
pub struct PtyBuilder {
    pub(super) default_session: String,
    pub(super) argv: Vec<OsString>,
    pub(super) login_shell: bool,
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) scrollback_lines: usize,
    pub(super) poll_interval: Duration,
    pub(super) stability_threshold: usize,
    pub(super) fallback_cwd: PathBuf,
    pub(super) fallback_shell: String,
    pub(super) env: HashMap<OsString, OsString>,
}

impl PtyBuilder {
    /// Creates a builder whose initial session will be named `default_session`.
    pub fn new(default_session: impl Into<String>) -> Self {
        Self {
            default_session: default_session.into(),
            argv: vec![OsString::from("bash")],
            login_shell: true,
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            poll_interval: DEFAULT_POLL_INTERVAL,
            stability_threshold: DEFAULT_STABILITY_THRESHOLD,
            fallback_cwd: PathBuf::from(DEFAULT_FALLBACK_CWD),
            fallback_shell: DEFAULT_FALLBACK_SHELL.to_owned(),
            env: HashMap::new(),
        }
    }

    /// Overrides the program argv. `argv[0]` is the executable path.
    ///
    /// Default: `["bash"]`.
    pub fn argv(mut self, argv: Vec<OsString>) -> Self {
        self.argv = argv;
        self
    }

    /// Sets whether `--login` (or `--noprofile --norc` in clean-env mode) is
    /// appended to the shell argv.
    ///
    /// Default: `true`.
    pub fn login_shell(mut self, login: bool) -> Self {
        self.login_shell = login;
        self
    }

    /// Overrides the PTY terminal dimensions (rows x cols).
    ///
    /// Wider columns capture long lines without wrapping.
    /// Default: `rows = 24`, `cols = 500`.
    pub fn dimensions(mut self, rows: u16, cols: u16) -> Self {
        self.rows = rows;
        self.cols = cols;
        self
    }

    /// Overrides the maximum number of scrollback lines retained per session.
    ///
    /// Higher values preserve more output at the cost of memory.
    /// Default: `10_000`.
    pub fn scrollback_lines(mut self, lines: usize) -> Self {
        self.scrollback_lines = lines;
        self
    }

    /// Overrides the sleep duration between output-polling reads.
    ///
    /// Lower values reduce latency at the cost of CPU usage.
    /// Default: `100 ms`.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Overrides the number of consecutive identical reads before output is
    /// considered stable.
    ///
    /// Lower values make completion detection faster but more fragile.
    /// Default: `3`.
    pub fn stability_threshold(mut self, threshold: usize) -> Self {
        self.stability_threshold = threshold;
        self
    }

    /// Overrides the fallback working directory used when the caller provides
    /// none and `std::env::current_dir()` fails.
    ///
    /// Default: `"/tmp"`.
    pub fn fallback_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.fallback_cwd = cwd.into();
        self
    }

    /// Overrides the fallback `$SHELL` value used in clean-env mode when the
    /// `SHELL` environment variable is unset.
    ///
    /// Default: `"/bin/bash"`.
    pub fn fallback_shell(mut self, shell: impl Into<String>) -> Self {
        self.fallback_shell = shell.into();
        self
    }

    /// Adds an environment variable to inject into every shell session.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Adds multiple environment variables to inject into every shell session.
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

    /// Validates the builder state and constructs a [`PtyBackend`](super::PtyBackend).
    ///
    /// Creates the initial session named by [`new`](Self::new).
    pub async fn build(self) -> Result<PtyBackend, ShellError> {
        self.validate()?;
        let mut backend = PtyBackend {
            sessions: HashMap::new(),
            current_session: String::new(),
            config: self,
            next_sentinel: 0,
        };
        let name = backend.config.default_session.clone();
        backend.create_session_internal(&name, None, false).await?;
        backend.current_session = name;
        Ok(backend)
    }

    /// Validates the configuration, returning an error if any field is invalid.
    ///
    /// These checks prevent values that would compile but cause subtle
    /// runtime failures:
    ///
    /// - `poll_interval = 0` — busy-loop that pins a CPU core
    /// - `scrollback_lines = 0` — all output silently discarded
    /// - `stability_threshold = 0` — premature command-completion detection
    /// - `rows = 0` / `cols = 0` — platform-dependent PTY misbehavior
    /// - empty `argv` — undefined spawn behavior
    pub fn validate(&self) -> Result<(), ShellError> {
        if self.argv.is_empty() {
            return Err(ShellError::backend("argv must not be empty"));
        }
        if self.rows == 0 {
            return Err(ShellError::backend("rows must be > 0"));
        }
        if self.cols == 0 {
            return Err(ShellError::backend("cols must be > 0"));
        }
        if self.scrollback_lines == 0 {
            return Err(ShellError::backend("scrollback_lines must be > 0"));
        }
        if self.poll_interval.is_zero() {
            return Err(ShellError::backend("poll_interval must be > 0"));
        }
        if self.stability_threshold == 0 {
            return Err(ShellError::backend("stability_threshold must be > 0"));
        }
        Ok(())
    }
}
