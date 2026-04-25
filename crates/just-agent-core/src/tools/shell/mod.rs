//! Reusable shell/session tools for LLM applications.
//!
//! This module provides shell/session tools so applications can opt into a shared,
//! provider-neutral tool runtime without bringing in a larger framework.
//! The `tools` feature enables:
//!
//! - command execution via [`ShellSessionExec`]
//! - output capture via [`ShellSessionCapture`]
//! - shell session lifecycle: [`ShellSessionList`], [`ShellSessionCreate`],
//!   [`ShellSessionSwitch`], [`ShellSessionKill`], [`ShellSessionRestart`]
//!
//! The tools share a [`ShellBackend`] abstraction, with [`PtyBackend`] as the default
//! persistent backend.

mod backend;
pub(crate) mod compat;
mod error;
pub mod session;

#[cfg(test)]
pub use backend::MockShellBackend;
pub use backend::{PtyBackend, PtyBuilder, SessionInfo, ShellBackend, ShellOutput};
pub use error::ShellError;
pub use session::*;

use std::sync::Arc;

use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

/// Creates a ready-to-register set of shell tools backed by the same shell backend.
///
/// Returns seven [`LlmTool`] implementations that share the provided `backend`.
/// The caller typically registers these with a [`ToolDispatcher`](crate::tools::ToolDispatcher).
pub fn shell_tool_set<B: ShellBackend + Send + Sync + 'static>(
    backend: Arc<Mutex<B>>,
) -> Vec<Box<dyn LlmTool>> {
    vec![
        Box::new(session::ShellSessionExec::new(backend.clone())),
        Box::new(session::ShellSessionCapture::new(backend.clone())),
        Box::new(session::ShellSessionList::new(backend.clone())),
        Box::new(session::ShellSessionCreate::new(backend.clone())),
        Box::new(session::ShellSessionSwitch::new(backend.clone())),
        Box::new(session::ShellSessionKill::new(backend.clone())),
        Box::new(session::ShellSessionRestart::new(backend)),
    ]
}

/// Creates a shell tool set backed by the in-memory mock backend for tests.
#[cfg(test)]
pub fn mock_shell_tool_set() -> (Vec<Box<dyn LlmTool>>, Arc<Mutex<MockShellBackend>>) {
    let backend = Arc::new(Mutex::new(MockShellBackend::new()));
    let tools = shell_tool_set(backend.clone());
    (tools, backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_tool_set_contains_all_tools() {
        let (tools, _) = mock_shell_tool_set();
        assert_eq!(tools.len(), 7);
    }
}
