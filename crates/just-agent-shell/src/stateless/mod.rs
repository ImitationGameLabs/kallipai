//! Stateless one-shot shell backend and tools.
//!
//! A parallel shell tool set that spawns a fresh `bash` process per command —
//! piped stdout/stderr, process-group tree-kill on timeout, a sticky working
//! directory recovered via a `pwd` roundtrip, an env snapshot replayed per call,
//! and an optional background-process supervisor. This is the execution model
//! used by Codex, Claude Code, and opencode (see
//! `.draft/design/shell-execution-stateless-redesign.md`).
//!
//! This module is **additive**: it lives alongside the existing persistent-PTY
//! backend ([`crate::backend::pty`]) and the seven [`crate::session`] tools,
//! which remain the tools the running agent uses. The stateless set is built
//! here for head-to-head comparison ([`compare`]) and a future swap-over; it is
//! intentionally **not** wired into `just-agent-runtime`.
//!
//! # Why stateless
//!
//! A persistent shell observed by screen-scraping the terminal makes cwd
//! correctness and background-command handling inseparable (both stem from one
//! shared mutable shell). Spawning a fresh process per call removes that root
//! cause: cwd is read fresh from `pwd` after every command (never a stale
//! cache), a hung command is one killed child (never a wedged session), and a
//! timed-out process group dies with the whole tree (never orphaned children).

pub mod backend;
pub mod builder;
pub mod cwd;
pub mod env_snapshot;
pub mod output;
pub mod pgroup;
pub mod supervisor;
pub mod tools;

#[cfg(any(test, feature = "testutils"))]
pub mod mock;

pub use tools::names;
