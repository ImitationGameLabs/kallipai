//! Shell-tool and backend errors.

use thiserror::Error;

/// Errors that can occur while operating shell tools.
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum ShellError {
    /// An error originated from the shell backend implementation.
    #[error("backend error: {reason}")]
    BackendError { reason: String },

    // -- background supervisor ----------------------------------------------
    /// A background task with the given id does not exist.
    #[error("background task '{task_id}' not found")]
    TaskNotFound { task_id: String },

    /// Killing the process group failed.
    #[error("failed to kill process group {pgid}: {reason}")]
    PgroupKillFailed { pgid: i32, reason: String },

    /// The inline command script exceeds the backend's size cap and was not
    /// passed to `bash -c`. The cap is set well below the kernel's
    /// `MAX_ARG_STRLEN` (128 KiB) on purpose, so that large content is routed
    /// to a file instead of an argv string. Stage the script on disk and run it
    /// as `bash <file>`.
    #[error(
        "inline command exceeds the {limit}-byte limit; write it to a file and run that instead"
    )]
    CommandTooLarge { limit: usize },

    /// A low-level I/O error occurred.
    #[error("I/O error: {0}")]
    Io(String),
}

impl ShellError {
    /// Creates a backend error.
    pub fn backend(reason: impl Into<String>) -> Self {
        Self::BackendError {
            reason: reason.into(),
        }
    }

    /// Creates a background-task-not-found error.
    pub fn task_not_found(task_id: impl Into<String>) -> Self {
        Self::TaskNotFound {
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

    /// Creates a command-too-large error for the given inline-script byte limit.
    pub fn command_too_large(limit: usize) -> Self {
        Self::CommandTooLarge { limit }
    }
}

impl From<std::io::Error> for ShellError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
