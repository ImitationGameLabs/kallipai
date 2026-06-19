//! Stateless shell tools: `bash_exec` plus background read/kill.
//!
//! Each tool is an [`LlmTool`](just_llm_client::tools::LlmTool) backed by a
//! [`StatelessBackend`](super::backend::StatelessBackend). Unlike the
//! [`crate::session`] tools there is no persistent session: every `bash_exec`
//! call spawns a fresh `bash` process.

mod bash_exec;
mod bg_kill;
mod bg_read;

pub use bash_exec::{BashExec, BashExecArgs, BashExecOutput};
pub use bg_kill::{BgKill, BgKillArgs, BgKillOutput};
pub use bg_read::{BgRead, BgReadArgs, BgReadOutput};

/// LLM-facing tool names — single source of truth.
///
/// Free constants (not associated constants) so they can be referenced without
/// a backend type parameter, mirroring [`crate::session::names`].
pub mod names {
    /// Execute a command in a fresh, isolated `bash` process.
    pub const BASH_EXEC: &str = "bash_exec";
    /// Read accumulated output (and status) from a background task.
    pub const BG_READ: &str = "bash_background_read";
    /// Kill a background task.
    pub const BG_KILL: &str = "bash_background_kill";
}
