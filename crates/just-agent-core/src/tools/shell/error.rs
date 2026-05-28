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
