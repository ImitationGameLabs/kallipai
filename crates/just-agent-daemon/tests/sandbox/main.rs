//! End-to-end sandbox / dirlock integration test.
//!
//! Spawns the **real** `just-agent-daemon` and `just-agent-run` binaries and drives
//! a scripted agent whose LLM is a wiremock standing in for the OpenAI-compatible
//! endpoint. Everything else -- profile loading, the bash sandbox
//! (mount-ns + landlock + seccomp), the `DirLockManager`, subagent creation via
//! the `just-agent` CLI, and append-only history -- runs exactly as in production.
//! Only the LLM HTTP call is mocked.
//!
//! Results are evaluated from the agents' history NDJSON (`<data>/just-agent/
//! agents/<id>/history/*.ndjson`) cross-checked against direct filesystem state.
//!
//! Layout:
//! - [`harness`] -- shared machinery (on-disk world, wiremock scripting, daemon
//!   + runner subprocess control, history/assertion helpers).
//! - [`guest`] / [`normal`] / [`dirlock`] -- the three scenario bodies, one per
//!   permission/dirlock concern.
//!
//!  1. **Guest** root agent -- secrets hidden (`.ssh` empty tmpfs), writes
//!     denied everywhere except the skills carve.
//!  2. **Normal** root agent -- workspace/home-lock/`/tmp` writable; daemon data
//!     tree read-only (read ok, write denied); skills carve writable; `.ssh` and
//!     `profiles.toml` readable (Normal has no hide-holes).
//!  3. **Subagent + dirlock** -- a child's nested workspace becomes a readonly
//!     hole to the parent (delegation carve), while the parent keeps writing its
//!     own workspace; a second subagent locking an overlapping path is rejected
//!     (409).
//!
//! Linux-only; gated behind the `sandbox-test` feature. Skip-guarded at runtime
//! when landlock or unprivileged user namespaces are unavailable.

#![cfg(all(target_os = "linux", feature = "sandbox-test"))]

mod dirlock;
mod guest;
mod harness;
mod normal;
