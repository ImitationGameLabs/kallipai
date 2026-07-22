//! Reusable shell tools for LLM applications.
//!
//! Provider-neutral shell tool runtime: applications opt into a shared
//! command-execution layer without bringing in a larger framework. Provides:
//!
//! - command execution via [`BashExec`]
//! - background-task read/kill via [`BgRead`] / [`BgKill`]
//!
//! All tools share a [`ShellBackend`] abstraction, with [`ProcessBackend`] as
//! the concrete implementation (a fresh `bash` process per command).
//!
//! # Testing
//!
//! Enable the `testutils` cargo feature for [`MockShellBackend`] and
//! [`mock_shell_tool_set`], which let downstream tests drive the shell tools
//! without spawning a real process.
//!
//! # Safety policy is the consumer's responsibility
//!
//! This crate provides shell tool *mechanism* only. Deciding whether a command
//! is safe to run (e.g. dangerous-command detection) is application policy and
//! is intentionally NOT bundled here; consumers wire their own classifier.
//!
//! # Platform
//!
//! Intentionally Unix-only: the backend (`pgroup`) uses `nix` process-group
//! signals, and the tagma/runtime build only on Unix. There is no Windows
//! build path today. This is deliberate, not a gap — `#[cfg(unix)]` gating is
//! omitted on purpose and will be added only if/when cross-platform support is
//! actually needed.

mod backend;
mod builder;
mod capture;
mod cwd;
mod error;
/// Linux-only landlock + mount-ns readonly-hole enforcement for spawned
/// processes. A thin composition layer: [`landlock::apply`] wires the owning
/// agent's directory-lock decision into the spawn-independent `prepare_*`/
/// `install_*` primitives of the `libsandbox` crate (landlock ruleset + mount-ns
/// readonly-holes), plus kallip's own seccomp denylist as the last step.
/// The backend is the caller that composes it.
#[cfg(all(target_os = "linux", feature = "landlock"))]
pub mod landlock;
#[cfg(any(test, feature = "testutils"))]
mod mock;
mod pgroup;
/// Linux-only seccomp denylist (defense-in-depth on top of landlock): blocks a
/// small set of rare high-risk syscalls. Sibling to `landlock`; layered on as
/// the last `pre_exec` step by `landlock::apply` when the feature is on.
#[cfg(all(target_os = "linux", feature = "seccomp"))]
pub mod seccomp;
mod supervisor;
pub mod tools;

use std::sync::Arc;

use just_llm_client::tools::LlmTool;
use tokio::sync::Mutex;

pub use backend::{CaptureMode, ProcessBackend, ShellBackend, ShellOutput};
pub use builder::ShellBuilder;
pub use error::ShellError;
pub use tools::{BashExec, BashExecOutput, BgKill, BgRead};

// The mock backend is public API behind the `testutils` feature so downstream
// crates can drive shell-tool tests without a real process. It is also compiled
// during this crate's own tests.
#[cfg(any(test, feature = "testutils"))]
pub use mock::MockShellBackend;

/// Creates a ready-to-register set of shell tools backed by the same
/// [`ShellBackend`].
///
/// Returns three [`LlmTool`] implementations: `bash_exec`,
/// `bash_background_read`, `bash_background_kill`. The caller typically
/// registers these with a [`ToolDispatcher`](just_llm_client::ToolDispatcher).
pub fn shell_tool_set<B: ShellBackend + Send + Sync + 'static>(
    backend: Arc<Mutex<B>>,
) -> Vec<Box<dyn LlmTool>> {
    vec![
        Box::new(BashExec::new(backend.clone())),
        Box::new(BgRead::new(backend.clone())),
        Box::new(BgKill::new(backend)),
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
    use crate::tools::names;

    #[test]
    fn shell_tool_set_contains_three_tools() {
        let (tools, _) = mock_shell_tool_set();
        assert_eq!(tools.len(), 3);
    }

    /// The base system prompt is lean (tool mechanics live here, not in the
    /// prompt). Guard the migrated content: bash_exec's description teaches the
    /// non-sticky cwd, and bg_read's teaches reacting to the completion notice.
    #[test]
    fn tool_descriptions_cover_usage_migrated_from_the_prompt() {
        let (tools, _) = mock_shell_tool_set();
        let desc = |name: &str| {
            tools
                .iter()
                .find(|t| t.name() == name)
                .map(|t| t.description().to_string())
                .unwrap_or_else(|| panic!("tool {name} present"))
        };
        let bash = desc(names::BASH_EXEC);
        // cwd is sticky and the returned value is authoritative -- the description
        // must not claim otherwise.
        assert!(
            bash.contains("persists across calls"),
            "bash_exec desc: {bash}"
        );
        assert!(bash.contains("authoritative"), "bash_exec desc: {bash}");
        let read = desc(names::BG_READ);
        assert!(read.contains("Background task"), "bg_read desc: {read}");
    }
}
