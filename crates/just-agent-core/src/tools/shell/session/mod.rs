//! Shell session management tools.
//!
//! Each sub-module implements an [`LlmTool`](just_llm_client::LlmTool) that maps to
//! a single [`ShellBackend`](super::backend::ShellBackend) method, exposing session
//! lifecycle operations (create, list, switch, kill, restart) and output capture
//! to the LLM.

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
