//! Shell session management tools.
//!
//! Each sub-module implements an [`LlmTool`](just_llm_client::LlmTool) that maps to
//! a single [`ShellBackend`](super::backend::ShellBackend) method, exposing command
//! execution, output capture, and session lifecycle operations (create, list,
//! switch, kill, restart) to the LLM.

mod capture;
mod create;
mod exec;
mod kill;
mod list;
mod restart;
mod switch;

pub use capture::{CaptureArgs, CaptureOutput, ShellSessionCapture};
pub use create::{CreateArgs, CreateOutput, ShellSessionCreate};
pub use exec::{ExecArgs, ExecOutput, ShellSessionExec};
pub use kill::{KillArgs, KillOutput, ShellSessionKill};
pub use list::{ListArgs, ListOutput, ShellSessionList};
pub use restart::{RestartArgs, RestartOutput, ShellSessionRestart};
pub use switch::{ShellSessionSwitch, SwitchArgs, SwitchOutput};

/// LLM-facing tool names — single source of truth.
///
/// Referenced by the policy layer (and any consumer) instead of duplicated
/// string literals. These are free constants rather than associated constants
/// because the tool structs are generic over `B: ShellBackend`, and associated
/// constants on a generic type can't be referenced without specifying `B`.
pub mod names {
    pub const EXEC: &str = "shell_session_exec";
    pub const CAPTURE: &str = "shell_session_capture";
    pub const LIST: &str = "shell_session_list";
    pub const CREATE: &str = "shell_session_create";
    pub const SWITCH: &str = "shell_session_switch";
    pub const KILL: &str = "shell_session_kill";
    pub const RESTART: &str = "shell_session_restart";
}
