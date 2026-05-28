//! PTY-backed shell backend using `portable-pty`.
//!
//! Cross-platform persistent shell backend that requires no external
//! binary. Each session is an independent PTY pair with its own shell process.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use strip_ansi_escapes::strip as strip_ansi;
use tokio::time::{sleep, timeout};

use super::super::compat::strip_common_prefix;
use super::super::error::ShellError;
use super::{SessionInfo, ShellBackend, ShellOutput};

// ---------------------------------------------------------------------------
// ScrollbackBuffer
// ---------------------------------------------------------------------------

/// Line-oriented ring buffer that accumulates PTY output.
struct ScrollbackBuffer {
    lines: Vec<String>,
    max_lines: usize,
}

impl ScrollbackBuffer {
    fn new(max_lines: usize) -> Self {
        Self {
            lines: Vec::with_capacity(1024),
            max_lines,
        }
    }

    fn append_line(&mut self, line: &str) {
        self.lines.push(line.to_owned());
        if self.lines.len() > self.max_lines {
            let excess = self.lines.len() - self.max_lines;
            self.lines.drain(..excess);
        }
    }

    /// Returns the last `n` lines joined with `\n`.
    fn last_n(&self, n: usize) -> String {
        let start = self.lines.len().saturating_sub(n);
        self.lines[start..].join("\n")
    }

    /// Returns the full buffer joined with `\n`.
    fn snapshot(&self) -> String {
        self.lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// PtySession
// ---------------------------------------------------------------------------

/// State for a single PTY-backed shell session.
///
/// Each session owns its own PTY master/slave pair, a background reader thread
/// that fills the scrollback buffer, and a handle to the child shell process.
/// Call [`shutdown`](PtySession::shutdown) to cleanly tear down these resources.
struct PtySession {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    scrollback: Arc<Mutex<ScrollbackBuffer>>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
    cwd: PathBuf,
}

impl PtySession {
    /// Kill the child process, drop the master (which terminates the reader
    /// thread), and join the reader thread.
    fn shutdown(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
        // Dropping master causes the reader's `read()` to error out.
        self.master.lock().unwrap().take();
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// PtyBuilder
// ---------------------------------------------------------------------------

// Default values matching previous hardcoded constants.
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 500;
const DEFAULT_SCROLLBACK_LINES: usize = 10_000;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_STABILITY_THRESHOLD: usize = 3;
const DEFAULT_FALLBACK_CWD: &str = "/tmp";
const DEFAULT_FALLBACK_SHELL: &str = "/bin/bash";

/// Builder for [`PtyBackend`].
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
/// use just_llm_client::tools::shell::PtyBuilder;
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
    default_session: String,
    argv: Vec<OsString>,
    login_shell: bool,
    rows: u16,
    cols: u16,
    scrollback_lines: usize,
    poll_interval: Duration,
    stability_threshold: usize,
    fallback_cwd: PathBuf,
    fallback_shell: String,
    env: HashMap<OsString, OsString>,
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

    /// Validates the builder state and constructs a [`PtyBackend`].
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

// ---------------------------------------------------------------------------
// PtyBackend
// ---------------------------------------------------------------------------

/// Persistent shell backend implemented on top of native PTY sessions.
///
/// This backend requires no external terminal multiplexer and works on Linux,
/// macOS, and Windows.
pub struct PtyBackend {
    sessions: HashMap<String, PtySession>,
    current_session: String,
    config: PtyBuilder,
    next_sentinel: u64,
}

impl PtyBackend {
    /// Exit-code marker appended after every command.
    const EC_PREFIX: &'static str = "__JUST_EC__:";
    /// Output-start sentinel prepended before every command.
    const START_PREFIX: &'static str = "__JUST_OUT_S__:";

    fn generate_sentinel(&mut self) -> String {
        let s = format!("{:08x}", self.next_sentinel);
        self.next_sentinel += 1;
        s
    }

    // -- private helpers ----------------------------------------------------

    fn get_session(&self, name: &str) -> Result<&PtySession, ShellError> {
        self.sessions
            .get(name)
            .ok_or_else(|| ShellError::session_not_found(name))
    }

    async fn create_session_internal(
        &mut self,
        name: &str,
        cwd: Option<&Path>,
        clean_env: bool,
    ) -> Result<(), ShellError> {
        let cwd = cwd.map(|p| p.to_path_buf()).unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| self.config.fallback_cwd.to_path_buf())
        });

        let mut cmd = CommandBuilder::from_argv(self.config.argv.clone());
        if clean_env {
            let shell =
                std::env::var("SHELL").unwrap_or_else(|_| self.config.fallback_shell.clone());
            if self.config.login_shell {
                cmd.arg("--noprofile");
                cmd.arg("--norc");
            }
            cmd.env_clear();
            if let Ok(home) = std::env::var("HOME") {
                cmd.env("HOME", &home);
            }
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", &path);
            }
            cmd.env("SHELL", &shell);
        } else if self.config.login_shell {
            cmd.arg("--login");
        }

        // Disable color output from all programs.
        cmd.env("TERM", "dumb");
        cmd.env("NO_COLOR", "1");
        cmd.env("LS_COLORS", "");
        cmd.env("CLICOLOR", "0");

        // Inject user-configured environment variables.
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }

        cmd.cwd(&cwd);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: self.config.rows,
                cols: self.config.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ShellError::session_create_failed(name, e.to_string()))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ShellError::session_create_failed(name, e.to_string()))?;

        // Take writer immediately — can only be called once.
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ShellError::session_create_failed(name, e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ShellError::session_create_failed(name, e.to_string()))?;

        let scrollback = Arc::new(Mutex::new(ScrollbackBuffer::new(
            self.config.scrollback_lines,
        )));
        let reader_handle = spawn_reader(reader, scrollback.clone());

        let session = PtySession {
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            scrollback,
            reader_handle: Some(reader_handle),
            cwd: cwd.clone(),
        };

        self.sessions.insert(name.to_owned(), session);
        sleep(self.config.poll_interval).await;

        // Suppress prompt and input echo for clean command output.
        let session = self.get_session(name)?;
        self.write_to_session(session, b"export PS1='' PS2=''; stty -echo 2>/dev/null\n")?;
        sleep(self.config.poll_interval).await;

        Ok(())
    }

    /// Send a command to the current session's PTY writer.
    fn write_to_session(&self, session: &PtySession, data: &[u8]) -> Result<(), ShellError> {
        let mut writer = session
            .writer
            .lock()
            .map_err(|e| ShellError::Io(e.to_string()))?;
        writer
            .write_all(data)
            .map_err(|e| ShellError::Io(e.to_string()))?;
        writer.flush().map_err(|e| ShellError::Io(e.to_string()))
    }

    /// Poll the scrollback buffer until the exit-code marker appears and the
    /// output has been stable for the configured number of consecutive reads.
    async fn wait_for_completion(
        &self,
        session: &PtySession,
        timeout_duration: Duration,
    ) -> Result<String, ShellError> {
        let threshold = self.config.stability_threshold;
        let poll_interval = self.config.poll_interval;
        let mut last_output = String::new();
        let mut stable_checks = 0usize;

        let wait = async {
            loop {
                let output = session.scrollback.lock().unwrap().snapshot();
                let has_marker = output.contains(Self::EC_PREFIX);

                if has_marker && output == last_output {
                    stable_checks += 1;
                    if stable_checks >= threshold {
                        return Ok(output);
                    }
                } else {
                    stable_checks = 0;
                }

                last_output = output;
                sleep(poll_interval).await;
            }
        };

        match timeout(timeout_duration, wait).await {
            Ok(result) => result,
            Err(_) => Err(ShellError::timeout(timeout_duration.as_secs())),
        }
    }

    /// Extract command output between start and end sentinel markers.
    fn extract_output(output: &str, sentinel: &str) -> (String, Option<i32>) {
        let start_marker = format!("{}{}", Self::START_PREFIX, sentinel);
        let mut exit_code = None;
        let mut in_range = false;
        let mut result_lines: Vec<&str> = Vec::new();

        for line in output.lines() {
            if line == start_marker {
                in_range = true;
                continue;
            }
            if let Some(rest) = line.strip_prefix(Self::EC_PREFIX) {
                exit_code = rest.trim().parse::<i32>().ok();
                break;
            }
            if in_range {
                // Fallback: strip echo artifacts when stty -echo is unavailable.
                if line.contains(&format!("echo {}", Self::START_PREFIX)) {
                    continue;
                }
                if line.contains(&format!("echo {}", Self::EC_PREFIX)) {
                    continue;
                }
                result_lines.push(line);
            }
        }

        (result_lines.join("\n"), exit_code)
    }
}

// ---------------------------------------------------------------------------
// Background reader thread
// ---------------------------------------------------------------------------

fn spawn_reader(
    reader: Box<dyn std::io::Read + Send>,
    scrollback: Arc<Mutex<ScrollbackBuffer>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut pending = String::new();

        loop {
            pending.clear();
            match reader.read_line(&mut pending) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let line = pending.strip_suffix('\n').unwrap_or(&pending);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    let clean = strip_ansi(line.as_bytes());
                    let line = String::from_utf8_lossy(&clean);
                    scrollback.lock().unwrap().append_line(&line);
                }
                Err(_) => break, // PTY closed
            }
        }
    })
}

// ---------------------------------------------------------------------------
// ShellBackend implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl ShellBackend for PtyBackend {
    async fn execute(
        &mut self,
        command: &str,
        timeout_duration: Duration,
        background: bool,
    ) -> Result<ShellOutput, ShellError> {
        let session_name = self.current_session.clone();
        let sentinel = self.generate_sentinel();
        let session = self.get_session(&session_name)?;

        let before = session.scrollback.lock().unwrap().snapshot();

        // Send start sentinel + command + exit-code marker.
        let payload = format!(
            "echo {}{}\n{}\necho {}$?\n",
            Self::START_PREFIX,
            sentinel,
            command,
            Self::EC_PREFIX,
        );
        self.write_to_session(session, payload.as_bytes())?;

        if background {
            return Ok(ShellOutput {
                output: String::new(),
                exit_code: None,
                timed_out: false,
            });
        }

        let full_output = match self.wait_for_completion(session, timeout_duration).await {
            Ok(output) => output,
            Err(ShellError::Timeout { .. }) => {
                return Ok(ShellOutput {
                    output: String::new(),
                    exit_code: None,
                    timed_out: true,
                });
            }
            Err(error) => return Err(error),
        };

        let diffed = strip_common_prefix(&before, &full_output);
        let (output, exit_code) = Self::extract_output(&diffed, &sentinel);

        Ok(ShellOutput {
            output,
            exit_code,
            timed_out: false,
        })
    }
    async fn capture_output(&mut self, lines: usize) -> Result<String, ShellError> {
        let session_name = self.current_session.clone();
        let session = self.get_session(&session_name)?;

        Ok(session.scrollback.lock().unwrap().last_n(lines))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, ShellError> {
        Ok(self
            .sessions
            .iter()
            .map(|(name, session)| SessionInfo {
                name: name.clone(),
                cwd: session.cwd.to_string_lossy().into_owned(),
                is_current: name == &self.current_session,
                window_count: 1,
            })
            .collect())
    }

    async fn create_session(&mut self, name: &str, cwd: Option<&Path>) -> Result<(), ShellError> {
        if self.sessions.contains_key(name) {
            return Err(ShellError::session_exists(name));
        }
        self.create_session_internal(name, cwd, false).await
    }

    async fn switch_session(&mut self, name: &str) -> Result<(), ShellError> {
        if !self.sessions.contains_key(name) {
            return Err(ShellError::session_not_found(name));
        }
        self.current_session = name.to_owned();
        Ok(())
    }

    async fn kill_session(&mut self, name: &str) -> Result<(), ShellError> {
        if !self.sessions.contains_key(name) {
            return Err(ShellError::session_not_found(name));
        }

        let mut session = self.sessions.remove(name).unwrap();
        session.shutdown();

        if self.current_session == name {
            self.current_session = self
                .sessions
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| self.config.default_session.clone());
        }

        Ok(())
    }

    async fn restart_session(&mut self, name: &str, clean_env: bool) -> Result<(), ShellError> {
        if !self.sessions.contains_key(name) {
            return Err(ShellError::session_not_found(name));
        }

        let was_current = self.current_session == name;
        // Remember the cwd before killing.
        let cwd = self.sessions.get(name).unwrap().cwd.clone();

        self.kill_session(name).await?;
        self.create_session_internal(name, Some(&cwd), clean_env)
            .await?;

        if was_current {
            self.current_session = name.to_owned();
        }

        Ok(())
    }

    fn current_session(&self) -> &str {
        &self.current_session
    }
}

impl Drop for PtyBackend {
    fn drop(&mut self) {
        for (_, mut session) in self.sessions.drain() {
            session.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PtyBackend>();
    }

    #[test]
    fn scrollback_last_n_returns_recent_lines() {
        let mut buf = ScrollbackBuffer::new(100);
        for i in 0..5 {
            buf.append_line(&format!("line{i}"));
        }
        assert_eq!(buf.last_n(3), "line2\nline3\nline4");
    }

    #[test]
    fn scrollback_trims_oldest_on_overflow() {
        let mut buf = ScrollbackBuffer::new(3);
        for i in 0..5 {
            buf.append_line(&format!("line{i}"));
        }
        assert_eq!(buf.snapshot(), "line2\nline3\nline4");
    }

    #[test]
    fn extract_output_returns_content_between_sentinels() {
        let output = "__JUST_OUT_S__:00000001\nhello\nworld\n__JUST_EC__:0\n";
        let (cleaned, code) = PtyBackend::extract_output(output, "00000001");
        assert_eq!(code, Some(0));
        assert_eq!(cleaned, "hello\nworld");
    }

    #[test]
    fn extract_output_returns_empty_when_markers_missing() {
        let output = "no markers here\n";
        let (cleaned, code) = PtyBackend::extract_output(output, "00000001");
        assert_eq!(code, None);
        assert_eq!(cleaned, "");
    }

    #[test]
    fn extract_output_strips_echo_artifacts_as_fallback() {
        let output = "bash-5.3$ echo __JUST_OUT_S__:00000001\n__JUST_OUT_S__:00000001\nbash-5.3$ mkdir foo\nfoo created\nbash-5.3$ echo __JUST_EC__:0\n__JUST_EC__:0\n";
        let (cleaned, code) = PtyBackend::extract_output(output, "00000001");
        assert_eq!(code, Some(0));
        assert_eq!(cleaned, "bash-5.3$ mkdir foo\nfoo created");
    }

    #[test]
    fn extract_output_with_nonzero_exit_code() {
        let output = "__JUST_OUT_S__:00000042\nerror: something failed\n__JUST_EC__:1\n";
        let (cleaned, code) = PtyBackend::extract_output(output, "00000042");
        assert_eq!(code, Some(1));
        assert_eq!(cleaned, "error: something failed");
    }

    #[test]
    fn pty_builder_default_matches_hardcoded_values() {
        let c = PtyBuilder::new("test");
        assert_eq!(c.argv, &[OsString::from("bash")]);
        assert!(c.login_shell);
        assert_eq!(c.rows, 24);
        assert_eq!(c.cols, 500);
        assert_eq!(c.scrollback_lines, 10_000);
        assert_eq!(c.poll_interval, Duration::from_millis(100));
        assert_eq!(c.stability_threshold, 3);
        assert_eq!(c.fallback_cwd, Path::new("/tmp"));
        assert_eq!(c.fallback_shell, "/bin/bash");
    }

    #[test]
    fn pty_builder_chains_correctly() {
        let c = PtyBuilder::new("test")
            .argv(vec![OsString::from("zsh")])
            .login_shell(false)
            .dimensions(40, 1000)
            .scrollback_lines(50_000)
            .poll_interval(Duration::from_millis(50))
            .stability_threshold(5)
            .fallback_cwd("/home")
            .fallback_shell("/bin/zsh");
        assert_eq!(c.argv, &[OsString::from("zsh")]);
        assert!(!c.login_shell);
        assert_eq!(c.rows, 40);
        assert_eq!(c.cols, 1000);
        assert_eq!(c.scrollback_lines, 50_000);
        assert_eq!(c.poll_interval, Duration::from_millis(50));
        assert_eq!(c.stability_threshold, 5);
        assert_eq!(c.fallback_cwd, Path::new("/home"));
        assert_eq!(c.fallback_shell, "/bin/zsh");
    }

    #[test]
    fn pty_builder_validate_rejects_invalid() {
        assert!(PtyBuilder::new("test").argv(vec![]).validate().is_err());
        assert!(
            PtyBuilder::new("test")
                .dimensions(0, 500)
                .validate()
                .is_err()
        );
        assert!(
            PtyBuilder::new("test")
                .dimensions(24, 0)
                .validate()
                .is_err()
        );
        assert!(
            PtyBuilder::new("test")
                .scrollback_lines(0)
                .validate()
                .is_err()
        );
        assert!(
            PtyBuilder::new("test")
                .poll_interval(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            PtyBuilder::new("test")
                .stability_threshold(0)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn pty_builder_validate_accepts_defaults() {
        assert!(PtyBuilder::new("test").validate().is_ok());
    }
}
