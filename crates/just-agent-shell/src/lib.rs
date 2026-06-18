//! Reusable shell/session tools for LLM applications.
//!
//! Provider-neutral shell tool runtime: applications opt into a shared
//! command-execution and session-management layer without bringing in a larger
//! framework. Provides:
//!
//! - command execution via [`ShellSessionExec`](session::ShellSessionExec)
//! - output capture via [`ShellSessionCapture`](session::ShellSessionCapture)
//! - shell session lifecycle: [`ShellSessionList`](session::ShellSessionList),
//!   [`ShellSessionCreate`](session::ShellSessionCreate),
//!   [`ShellSessionSwitch`](session::ShellSessionSwitch),
//!   [`ShellSessionKill`](session::ShellSessionKill),
//!   [`ShellSessionRestart`](session::ShellSessionRestart)
//!
//! All tools share a [`ShellBackend`] abstraction, with [`PtyBackend`] as the
//! default persistent backend.
//!
//! # Testing
//!
//! Enable the `testutils` cargo feature for `MockShellBackend` and
//! `mock_shell_tool_set`, which let downstream tests drive the shell tools
//! without spawning a real PTY.
//!
//! # Safety policy is the consumer's responsibility
//!
//! This crate provides shell tool *mechanism* only. Deciding whether a command
//! is safe to run (e.g. dangerous-command detection) is application policy and
//! is intentionally NOT bundled here; consumers wire their own classifier.

mod backend;
// Private helper module for the PTY backend. `strip_common_prefix` is exposed
// only as `pub(crate)`; keeping the module private prevents accidental leakage.
mod compat;
mod error;
pub mod session;

use std::sync::Arc;

use just_llm_client::tools::LlmTool;
use tokio::sync::Mutex;

pub use backend::{PtyBackend, PtyBuilder, SessionInfo, ShellBackend, ShellOutput};
pub use error::ShellError;

// The mock backend is public API behind the `testutils` feature so downstream
// crates can drive shell-tool tests without a real PTY. It is also compiled
// during this crate's own tests.
#[cfg(any(test, feature = "testutils"))]
pub use backend::MockShellBackend;

/// Creates a ready-to-register set of shell tools backed by the same shell backend.
///
/// Returns seven [`LlmTool`] implementations that share the provided `backend`.
/// The caller typically registers these with a [`ToolDispatcher`](just_llm_client::ToolDispatcher).
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
///
/// Available with the `testutils` feature (and during this crate's own tests).
#[cfg(any(test, feature = "testutils"))]
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
