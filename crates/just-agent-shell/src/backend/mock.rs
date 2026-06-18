use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    time::Duration,
};

use async_trait::async_trait;

use super::super::error::ShellError;
use super::{SessionInfo, ShellBackend, ShellOutput};

/// In-memory shell backend for tests; available behind the `testutils` feature.
#[derive(Debug, Default)]
pub struct MockShellBackend {
    sessions: HashMap<String, MockSession>,
    current: String,
    next_outputs: VecDeque<String>,
    next_exit_codes: VecDeque<i32>,
    should_timeout: bool,
    default_output: String,
    default_exit_code: i32,
}

#[derive(Debug, Clone)]
struct MockSession {
    name: String,
    cwd: String,
    env: HashMap<String, String>,
    history: Vec<String>,
    pending_output: VecDeque<String>,
}

impl MockSession {
    fn new(name: &str, cwd: &str) -> Self {
        Self {
            name: name.to_owned(),
            cwd: cwd.to_owned(),
            env: HashMap::new(),
            history: Vec::new(),
            pending_output: VecDeque::new(),
        }
    }
}

impl MockShellBackend {
    /// Creates a backend with a default `main` session.
    pub fn new() -> Self {
        let cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/tmp".to_owned());
        let mut sessions = HashMap::new();
        sessions.insert("main".to_owned(), MockSession::new("main", &cwd));

        Self {
            sessions,
            current: "main".to_owned(),
            next_outputs: VecDeque::new(),
            next_exit_codes: VecDeque::new(),
            should_timeout: false,
            default_output: String::new(),
            default_exit_code: 0,
        }
    }

    /// Queues output to be returned by the next [`execute`](ShellBackend::execute) call.
    pub fn push_output(&mut self, output: impl Into<String>) -> &mut Self {
        self.next_outputs.push_back(output.into());
        self
    }

    /// Queues an exit code to be returned by the next [`execute`](ShellBackend::execute) call.
    pub fn push_exit_code(&mut self, code: i32) -> &mut Self {
        self.next_exit_codes.push_back(code);
        self
    }

    /// Configures whether the next [`execute`](ShellBackend::execute) call simulates a timeout.
    pub fn set_should_timeout(&mut self, should_timeout: bool) -> &mut Self {
        self.should_timeout = should_timeout;
        self
    }

    /// Sets the fallback output returned when no queued output remains.
    pub fn set_default_output(&mut self, output: impl Into<String>) -> &mut Self {
        self.default_output = output.into();
        self
    }

    /// Sets the fallback exit code returned when no queued code remains.
    pub fn set_default_exit_code(&mut self, code: i32) -> &mut Self {
        self.default_exit_code = code;
        self
    }

    /// Adds a new named session to the backend.
    pub fn add_session(&mut self, name: &str) -> &mut Self {
        self.sessions
            .insert(name.to_owned(), MockSession::new(name, "/tmp"));
        self
    }

    /// Sets the currently focused session.
    pub fn set_current(&mut self, name: &str) -> &mut Self {
        self.current = name.to_owned();
        self
    }

    /// Sets an environment variable for the given session.
    pub fn set_env(&mut self, session: &str, key: &str, value: &str) -> &mut Self {
        if let Some(session) = self.sessions.get_mut(session) {
            session.env.insert(key.to_owned(), value.to_owned());
        }
        self
    }

    /// Returns whether the given session has the specified environment variable.
    pub fn has_env(&self, session: &str, key: &str) -> bool {
        self.sessions
            .get(session)
            .is_some_and(|session| session.env.contains_key(key))
    }

    /// Returns whether a session with the given name exists.
    pub fn has_session(&self, name: &str) -> bool {
        self.sessions.contains_key(name)
    }

    /// Returns the total number of sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Returns the commands executed in `session`, in invocation order.
    ///
    /// Empty if the session does not exist or has run no commands. The primary
    /// assertion hook for tests driving shell tools: verify *what* an agent
    /// actually ran — or that a denied/pending command never reached the backend.
    pub fn executed_commands(&self, session: &str) -> Vec<String> {
        self.sessions
            .get(session)
            .map(|s| s.history.clone())
            .unwrap_or_default()
    }

    /// Sets the pending output lines for a specific session (used by [`capture_output`](ShellBackend::capture_output)).
    pub fn set_session_output(&mut self, session: &str, lines: Vec<&str>) -> &mut Self {
        if let Some(session) = self.sessions.get_mut(session) {
            session.pending_output = lines.into_iter().map(String::from).collect();
        }
        self
    }
}

#[async_trait]
impl ShellBackend for MockShellBackend {
    async fn execute(
        &mut self,
        command: &str,
        _timeout: Duration,
        background: bool,
    ) -> Result<ShellOutput, ShellError> {
        if !self.sessions.contains_key(&self.current) {
            return Err(ShellError::session_not_found(&self.current));
        }

        // Record before the timeout/background short-circuits — see
        // `executed_commands_tracks_invocations_per_session` for why both count.
        if let Some(session) = self.sessions.get_mut(&self.current) {
            session.history.push(command.to_owned());
        }

        if self.should_timeout {
            self.should_timeout = false;
            return Ok(ShellOutput {
                output: String::new(),
                exit_code: None,
                timed_out: true,
            });
        }

        if background {
            return Ok(ShellOutput {
                output: String::new(),
                exit_code: None,
                timed_out: false,
            });
        }

        Ok(ShellOutput {
            output: self
                .next_outputs
                .pop_front()
                .unwrap_or_else(|| self.default_output.clone()),
            exit_code: Some(
                self.next_exit_codes
                    .pop_front()
                    .unwrap_or(self.default_exit_code),
            ),
            timed_out: false,
        })
    }

    async fn capture_output(&mut self, lines: usize) -> Result<String, ShellError> {
        let session = self
            .sessions
            .get(&self.current)
            .ok_or_else(|| ShellError::session_not_found(&self.current))?;

        Ok(session
            .pending_output
            .iter()
            .rev()
            .take(lines)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, ShellError> {
        Ok(self
            .sessions
            .values()
            .map(|session| SessionInfo {
                name: session.name.clone(),
                cwd: session.cwd.clone(),
                is_current: session.name == self.current,
                window_count: 1,
            })
            .collect())
    }

    async fn create_session(&mut self, name: &str, cwd: Option<&Path>) -> Result<(), ShellError> {
        if self.sessions.contains_key(name) {
            return Err(ShellError::session_exists(name));
        }

        let cwd = cwd
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_owned());
        self.sessions
            .insert(name.to_owned(), MockSession::new(name, &cwd));
        Ok(())
    }

    async fn switch_session(&mut self, name: &str) -> Result<(), ShellError> {
        if !self.sessions.contains_key(name) {
            return Err(ShellError::session_not_found(name));
        }

        self.current = name.to_owned();
        Ok(())
    }

    async fn kill_session(&mut self, name: &str) -> Result<(), ShellError> {
        if !self.sessions.contains_key(name) {
            return Err(ShellError::session_not_found(name));
        }

        self.sessions.remove(name);
        if self.current == name {
            self.current = self
                .sessions
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "main".to_owned());
        }

        Ok(())
    }

    async fn restart_session(&mut self, name: &str, clean_env: bool) -> Result<(), ShellError> {
        if !self.sessions.contains_key(name) {
            return Err(ShellError::session_not_found(name));
        }
        // `history` is accumulated invocation state and resets on every restart,
        // mirroring PtyBackend's kill+recreate; only `env` is gated by `clean_env`.
        if let Some(session) = self.sessions.get_mut(name) {
            session.history.clear();
            if clean_env {
                session.env.clear();
            }
        }
        Ok(())
    }

    fn current_session(&self) -> &str {
        &self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    #[tokio::test]
    async fn mock_backend_tracks_sessions_and_io() {
        let mut backend = MockShellBackend::new();
        backend
            .add_session("worker")
            .set_session_output("worker", vec!["line1", "line2"]);
        backend.switch_session("worker").await.unwrap();
        assert_eq!(backend.capture_output(1).await.unwrap(), "line2");
    }

    #[tokio::test]
    async fn mock_backend_is_shareable_through_mutex() {
        let backend = Arc::new(AsyncMutex::new(MockShellBackend::new()));
        backend
            .lock()
            .await
            .execute("echo ok", Duration::from_secs(1), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn executed_commands_tracks_invocations_per_session() {
        let mut backend = MockShellBackend::new();
        backend.add_session("worker");
        backend
            .execute("ls", Duration::from_secs(1), false)
            .await
            .unwrap();
        backend
            .execute("echo hi", Duration::from_secs(1), false)
            .await
            .unwrap();
        backend.switch_session("worker").await.unwrap();
        backend
            .execute("pwd", Duration::from_secs(1), false)
            .await
            .unwrap();

        // Per-session attribution, no cross-talk.
        assert_eq!(
            backend.executed_commands("main"),
            vec!["ls".to_owned(), "echo hi".to_owned()]
        );
        assert_eq!(backend.executed_commands("worker"), vec!["pwd".to_owned()]);

        // Timed-out and background commands are still recorded: they reached
        // the backend (the real PtyBackend writes each command to the PTY
        // before short-circuiting).
        backend.set_should_timeout(true);
        backend
            .execute("sleep 999", Duration::from_secs(1), false)
            .await
            .unwrap();
        backend
            .execute("long-running", Duration::from_secs(1), true)
            .await
            .unwrap();
        assert_eq!(
            backend.executed_commands("worker"),
            vec![
                "pwd".to_owned(),
                "sleep 999".to_owned(),
                "long-running".to_owned(),
            ]
        );

        // Unknown session returns an empty Vec (no panic).
        assert!(backend.executed_commands("nope").is_empty());

        // restart clears history unconditionally (clean_env only gates env).
        backend.restart_session("worker", false).await.unwrap();
        assert!(backend.executed_commands("worker").is_empty());
    }
}
