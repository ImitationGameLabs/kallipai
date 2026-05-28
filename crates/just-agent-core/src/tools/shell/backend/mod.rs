//! Shell backend abstraction shared by shell tools.

mod pty;

#[cfg(test)]
#[allow(missing_docs)]
mod mock;

#[cfg(test)]
pub use mock::MockShellBackend;
pub use pty::{PtyBackend, PtyBuilder};

use std::{path::Path, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::error::ShellError;

/// Metadata about a shell session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session name.
    pub name: String,
    /// Session working directory.
    pub cwd: String,
    /// Whether this session is currently focused by the backend.
    pub is_current: bool,
    /// Number of windows tracked by the underlying backend.
    pub window_count: usize,
}

/// Output returned by a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    /// Combined stdout/stderr text.
    pub output: String,
    /// Exit status when available.
    pub exit_code: Option<i32>,
    /// Whether the command timed out.
    pub timed_out: bool,
}

/// Backend abstraction for persistent shell/session operations.
#[async_trait]
pub trait ShellBackend: Send + Sync {
    /// Executes a command in the current session.
    async fn execute(
        &mut self,
        command: &str,
        timeout: Duration,
        background: bool,
    ) -> Result<ShellOutput, ShellError>;

    /// Captures recent terminal output from the current session.
    async fn capture_output(&mut self, lines: usize) -> Result<String, ShellError>;

    /// Lists all sessions known to the backend.
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, ShellError>;

    /// Creates a new session.
    async fn create_session(&mut self, name: &str, cwd: Option<&Path>) -> Result<(), ShellError>;

    /// Switches focus to a different session.
    async fn switch_session(&mut self, name: &str) -> Result<(), ShellError>;

    /// Kills a session and its processes.
    async fn kill_session(&mut self, name: &str) -> Result<(), ShellError>;

    /// Restarts a session.
    async fn restart_session(&mut self, name: &str, clean_env: bool) -> Result<(), ShellError>;

    /// Returns the currently focused session name.
    fn current_session(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_info_and_output_are_plain_data() {
        let info = SessionInfo {
            name: "main".to_owned(),
            cwd: "/tmp".to_owned(),
            is_current: true,
            window_count: 1,
        };
        let output = ShellOutput {
            output: "ok".to_owned(),
            exit_code: Some(0),
            timed_out: false,
        };

        assert_eq!(info.name, "main");
        assert_eq!(output.exit_code, Some(0));
        assert!(!output.timed_out);
    }
}
