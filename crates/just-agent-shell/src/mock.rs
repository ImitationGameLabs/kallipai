//! In-memory mock of [`ShellBackend`] for tests (behind `testutils`).
//!
//! Queued outputs/exit codes, a timeout switch, and a recorded-command history.
//! The background surface is stubbed (it can't really spawn). It is a **stub,
//! not a simulation** — it does not track `cd`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::backend::{ShellBackend, ShellOutput};
use crate::error::ShellError;
use crate::supervisor::{BgReadOutput, TaskState};

/// Test double for [`ShellBackend`].
pub struct MockShellBackend {
    cwd: PathBuf,
    outputs: VecDeque<String>,
    exit_codes: VecDeque<Option<i32>>,
    should_timeout: bool,
    commands: Vec<String>,
    background: HashMap<String, String>,
    next_bg: AtomicU64,
}

impl MockShellBackend {
    /// Creates an empty mock whose cwd is the process cwd (or `/tmp`).
    pub fn new() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp")),
            outputs: VecDeque::new(),
            exit_codes: VecDeque::new(),
            should_timeout: false,
            commands: Vec::new(),
            background: HashMap::new(),
            next_bg: AtomicU64::new(0),
        }
    }

    /// Queues a stdout blob for the next `exec`.
    pub fn push_output(&mut self, output: impl Into<String>) -> &mut Self {
        self.outputs.push_back(output.into());
        self
    }

    /// Queues an exit code for the next `exec`.
    pub fn push_exit_code(&mut self, code: Option<i32>) -> &mut Self {
        self.exit_codes.push_back(code);
        self
    }

    /// Makes the next `exec` time out (exit 124).
    pub fn set_should_timeout(&mut self) -> &mut Self {
        self.should_timeout = true;
        self
    }

    /// Returns the commands recorded by `exec`, in order.
    pub fn executed_commands(&self) -> &[String] {
        &self.commands
    }
}

impl Default for MockShellBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ShellBackend for MockShellBackend {
    async fn exec(&mut self, command: &str, _timeout: Duration) -> Result<ShellOutput, ShellError> {
        self.commands.push(command.to_owned());

        if self.should_timeout {
            self.should_timeout = false;
            return Ok(ShellOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(124),
                timed_out: true,
                truncated: false,
                cwd: self.cwd.clone(),
            });
        }

        let stdout = self.outputs.pop_front().unwrap_or_default();
        let exit_code = self.exit_codes.pop_front().flatten();
        Ok(ShellOutput {
            stdout,
            stderr: String::new(),
            exit_code,
            timed_out: false,
            truncated: false,
            cwd: self.cwd.clone(),
        })
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    async fn spawn_background(&mut self, command: &str) -> Result<String, ShellError> {
        let id = self.next_bg.fetch_add(1, Ordering::Relaxed).to_string();
        self.background.insert(id.clone(), command.to_owned());
        Ok(id)
    }

    async fn read_background(
        &self,
        id: &str,
        _tail_bytes: usize,
    ) -> Result<BgReadOutput, ShellError> {
        if !self.background.contains_key(id) {
            return Err(ShellError::task_not_found(id));
        }
        Ok(BgReadOutput {
            output: String::new(),
            state: TaskState::Running,
            exit_code: None,
            stalled: false,
            bytes: 0,
        })
    }

    async fn kill_background(&mut self, id: &str) -> Result<(), ShellError> {
        if self.background.remove(id).is_none() {
            return Err(ShellError::task_not_found(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_returns_queued_output_and_records_command() {
        let mut backend = MockShellBackend::new();
        backend.push_output("hello").push_exit_code(Some(0));
        let out = backend
            .exec("echo hello", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
        assert_eq!(backend.executed_commands(), &["echo hello"]);
    }

    #[tokio::test]
    async fn timeout_returns_124() {
        let mut backend = MockShellBackend::new();
        backend.set_should_timeout();
        let out = backend
            .exec("sleep 999", Duration::from_millis(1))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, Some(124));
    }

    #[tokio::test]
    async fn background_round_trip() {
        let mut backend = MockShellBackend::new();
        let id = backend.spawn_background("build").await.unwrap();
        let read = backend.read_background(&id, 1024).await.unwrap();
        assert_eq!(read.state, TaskState::Running);
        backend.kill_background(&id).await.unwrap();
        let err = backend.read_background(&id, 1024).await.unwrap_err();
        assert!(matches!(err, ShellError::TaskNotFound { .. }));
    }
}
