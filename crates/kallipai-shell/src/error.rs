//! Shell-tool and backend errors.

use std::path::{Path, PathBuf};

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

    /// The spawn sequence was refused at a known step.
    #[error("{}", stage_message(.stage, .path, .detail))]
    SpawnFailed {
        stage: SpawnStage,
        path: Option<PathBuf>,
        detail: String,
    },

    /// The landlock ruleset could not be installed, so the spawn was abandoned.
    ///
    /// Kept apart from [`ShellError::SpawnFailed`]: nothing was filtered
    /// because the ruleset never took effect, and the operator fixes that by
    /// changing the sandbox configuration rather than the spawn step. Two
    /// producers reach it: a ruleset that could not be built before the fork,
    /// and one that could not be installed in the child. The name is the
    /// mechanism on purpose, and no path is named: the ruleset is the whole
    /// filesystem view at once.
    #[error("landlock refused the spawn: {detail}")]
    Landlock { detail: String },

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

/// The step of the spawn sequence that refused to continue.
///
/// Parent-side preparation and child-side installation of the same mechanism
/// share one name — `Bind` covers preparing and installing a bind mount —
/// because for the operator both mean "this mount is not there".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnStage {
    /// Entering a user mount namespace.
    MountNamespace,
    /// Applying a bind mount.
    Bind,
    /// Applying a tmpfs mount.
    Tmpfs,
    /// Installing a seccomp filter.
    Seccomp,
    /// Making the working directory the child's current directory.
    WorkingDirectory,
    /// Executing the program.
    Exec,
}

impl SpawnStage {
    /// Every stage, in wire order: the whole mapping from stage to code.
    ///
    /// A code is a position plus one, so a stage missing from the list can only
    /// lose its name, never borrow a neighbour's. Both directions go through
    /// this table; nothing else spells a code. The mapping is total: every
    /// variant is listed. The channel itself carries the four steps the child
    /// installs (mount-ns, bind, tmpfs, seccomp); the working directory and the
    /// exec are the parent's own findings, so they have codes but are never
    /// sent.
    const WIRE: [Self; 6] = [
        Self::MountNamespace,
        Self::Bind,
        Self::Tmpfs,
        Self::Seccomp,
        Self::WorkingDirectory,
        Self::Exec,
    ];

    /// The wire code for this stage, or `None` for a stage this build knows but
    /// [`SpawnStage::WIRE`] does not list: adding a variant without adding it to
    /// the table loses the name instead of borrowing a neighbour's code.
    pub(crate) fn code(self) -> Option<u8> {
        let position = Self::WIRE.iter().position(|stage| *stage == self)?;
        u8::try_from(position + 1).ok()
    }

    /// The stage a wire code names; an unknown code names no stage.
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        usize::from(code)
            .checked_sub(1)
            .and_then(|index| Self::WIRE.get(index))
            .copied()
    }
}

impl std::fmt::Display for SpawnStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::MountNamespace => "mount namespace",
            Self::Bind => "bind",
            Self::Tmpfs => "tmpfs",
            Self::Seccomp => "seccomp",
            Self::WorkingDirectory => "working directory",
            Self::Exec => "exec",
        };
        f.write_str(name)
    }
}

fn stage_message(stage: &SpawnStage, path: &Option<PathBuf>, detail: &str) -> String {
    let mut message = format!("spawn failed at {stage}");
    if let Some(path) = path {
        message.push_str(&format!(" for {}", path.display()));
    }
    message.push_str(": ");
    message.push_str(detail);
    message
}

/// A refusal the sandboxed child recorded before it died.
///
/// `None` stage means the landlock step refused (see [`ShellError::Landlock`],
/// which names no path); `None` path means the step named no path, or the
/// recorded index could not be trusted.
pub(crate) type RecordedRefusal = (Option<SpawnStage>, Option<PathBuf>, String);

/// Turns a `Command::spawn` failure into a structured error when the evidence
/// names a step, and keeps the plain I/O error when it does not.
///
/// Two sources, in this order:
///
/// 1. the child, which recorded the step and path it refused into the
///    out-of-band channel before returning its errno (`refused`);
/// 2. facts this side can check by itself: a working directory it cannot
///    enter, a program it cannot execute.
///
/// The spawn error alone names no step: `fork` carries one errno back, and an
/// errno is not a step name. When neither source names one the bare `Io` error
/// stands, because a wrong step name is worse than no step name.
pub(crate) fn explain_spawn(
    error: std::io::Error,
    cwd: &Path,
    program: &std::ffi::OsStr,
    refused: Option<RecordedRefusal>,
) -> ShellError {
    if let Some((stage, path, detail)) = refused {
        return match stage {
            Some(stage) => ShellError::SpawnFailed {
                stage,
                path,
                detail,
            },
            None => ShellError::Landlock { detail },
        };
    }
    let detail = error.to_string();
    if !cwd.is_dir() || !is_searchable(cwd) {
        return ShellError::SpawnFailed {
            stage: SpawnStage::WorkingDirectory,
            path: Some(cwd.to_path_buf()),
            detail,
        };
    }
    let program = Path::new(program);
    if program.is_absolute() {
        if !program.is_file() || !is_searchable(program) {
            return ShellError::SpawnFailed {
                stage: SpawnStage::Exec,
                path: Some(program.to_path_buf()),
                detail,
            };
        }
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    ) {
        // A relative program is resolved against `PATH` inside the child, and
        // both errnos come from that lookup or from the exec itself; the
        // program name is the only path this side can honestly name.
        return ShellError::SpawnFailed {
            stage: SpawnStage::Exec,
            path: Some(program.to_path_buf()),
            detail,
        };
    }
    ShellError::Io(detail)
}

/// Whether this process can search `path`, read-only: `access(2)` changes nothing.
///
/// Search permission alone does not make a path usable: a regular file passes
/// `X_OK` when its mode allows. Callers pair this with `is_dir`/`is_file` for
/// the kind of path they need.
pub(crate) fn is_searchable(path: &Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

#[cfg(test)]
mod tests;
