//! Shell-tool and backend errors.

use thiserror::Error;

/// Errors that can occur while operating shell tools.
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum ShellError {
    /// The command did not complete within the allowed duration.
    #[error("command timed out after {timeout}s")]
    Timeout { timeout: u64 },

    /// No session with the given name exists.
    #[error("session '{name}' not found")]
    SessionNotFound { name: String },

    /// A session with the given name already exists.
    #[error("session '{name}' already exists")]
    SessionExists { name: String },

    /// The shell command returned a non-zero exit or could not be started.
    #[error("command execution failed: {reason}")]
    ExecutionFailed { reason: String },

    /// An error originated from the shell backend implementation.
    #[error("backend error: {reason}")]
    BackendError { reason: String },

    /// A new session could not be created.
    #[error("failed to create session '{name}': {reason}")]
    SessionCreateFailed { name: String, reason: String },

    // -- stateless backend + background supervisor -------------------------
    /// A background task with the given id does not exist.
    #[error("background task '{task_id}' not found")]
    TaskNotFound { task_id: String },

    /// A background task's output exceeded the size limit and was killed.
    #[error("background task '{task_id}' output exceeded {limit} bytes")]
    OutputTooLarge { task_id: String, limit: usize },

    /// A background task appeared stalled on an interactive prompt.
    #[error("background task '{task_id}' stalled on an interactive prompt")]
    Stalled { task_id: String },

    /// Killing the process group failed.
    #[error("failed to kill process group {pgid}: {reason}")]
    PgroupKillFailed { pgid: i32, reason: String },

    /// The sticky working directory could not be resolved after a command.
    #[error("cwd resolution failed: {reason}")]
    CwdResolutionFailed { reason: String },

    /// The env snapshot could not be captured or read.
    #[error("env snapshot error: {reason}")]
    EnvSnapshot { reason: String },

    /// A low-level I/O error occurred.
    #[error("I/O error: {0}")]
    Io(String),
}

impl ShellError {
    /// Creates a timeout error.
    pub fn timeout(seconds: u64) -> Self {
        Self::Timeout { timeout: seconds }
    }

    /// Creates a session-not-found error.
    pub fn session_not_found(name: impl Into<String>) -> Self {
        Self::SessionNotFound { name: name.into() }
    }

    /// Creates a duplicate-session error.
    pub fn session_exists(name: impl Into<String>) -> Self {
        Self::SessionExists { name: name.into() }
    }

    /// Creates an execution-failed error.
    pub fn execution_failed(reason: impl Into<String>) -> Self {
        Self::ExecutionFailed {
            reason: reason.into(),
        }
    }

    /// Creates a backend error.
    pub fn backend(reason: impl Into<String>) -> Self {
        Self::BackendError {
            reason: reason.into(),
        }
    }

    /// Creates a session-create-failed error.
    pub fn session_create_failed(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::SessionCreateFailed {
            name: name.into(),
            reason: reason.into(),
        }
    }

    /// Creates a background-task-not-found error.
    pub fn task_not_found(task_id: impl Into<String>) -> Self {
        Self::TaskNotFound {
            task_id: task_id.into(),
        }
    }

    /// Creates an output-too-large error (background task killed for overflow).
    pub fn output_too_large(task_id: impl Into<String>, limit: usize) -> Self {
        Self::OutputTooLarge {
            task_id: task_id.into(),
            limit,
        }
    }

    /// Creates a stalled-on-interactive-prompt error.
    pub fn stalled(task_id: impl Into<String>) -> Self {
        Self::Stalled {
            task_id: task_id.into(),
        }
    }

    /// Creates a process-group-kill-failed error.
    pub fn pgroup_kill_failed(pgid: i32, reason: impl Into<String>) -> Self {
        Self::PgroupKillFailed {
            pgid,
            reason: reason.into(),
        }
    }

    /// Creates a cwd-resolution-failed error.
    pub fn cwd_resolution_failed(reason: impl Into<String>) -> Self {
        Self::CwdResolutionFailed {
            reason: reason.into(),
        }
    }

    /// Creates an env-snapshot error.
    pub fn env_snapshot(reason: impl Into<String>) -> Self {
        Self::EnvSnapshot {
            reason: reason.into(),
        }
    }
}

impl From<std::io::Error> for ShellError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_preserve_payloads() {
        assert!(matches!(
            ShellError::timeout(3),
            ShellError::Timeout { timeout: 3 }
        ));
        assert!(matches!(
            ShellError::session_not_found("main"),
            ShellError::SessionNotFound { name } if name == "main"
        ));
        assert!(matches!(
            ShellError::session_exists("main"),
            ShellError::SessionExists { name } if name == "main"
        ));
        assert!(matches!(
            ShellError::execution_failed("boom"),
            ShellError::ExecutionFailed { reason } if reason == "boom"
        ));
    }
}
