//! Process-group tree-kill for stateless command execution.
//!
//! [`tokio::process::Command::process_group`](tokio::process::Command::process_group)`(0)`
//! makes the spawned child the leader of a fresh process group (PGID == child
//! PID), so signalling that group reaches the *whole* tree — not just the
//! leader. Killing only the leader ([`Child::kill`](tokio::process::Child::kill))
//! orphans its children: the failure mode behind Claude Code's infamous
//! `rm -rf` logging incident.
//!
//! Uses `nix` for typed, `unsafe`-free signal delivery (`Pid`, `Signal`).
//!
//! Unix-only: nix is unix-only, and every caller of this module is unix-only
//! today (the daemon/runtime build only on unix). If cross-platform support is
//! ever added, reintroduce `#[cfg(unix)]` gating here.

use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::Child;

use crate::error::ShellError;

/// Grace period between SIGTERM and SIGKILL, mirroring the daemon's
/// `Agent::shutdown` two-phase shape.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Kill an entire process group, then reap the leader.
///
/// Phase 1: SIGTERM the group, wait up to [`KILL_GRACE`] for the child to exit.
/// Phase 2: if it did not, SIGKILL the group and reap. Returns `true` when the
/// child exited during the graceful phase.
pub(super) async fn kill_tree(child: &mut Child) -> Result<bool, ShellError> {
    let Some(pid) = child.id() else {
        return Ok(true); // already reaped
    };
    let pgid = Pid::from_raw(pid as i32);

    kill_group(pgid, Signal::SIGTERM)?;
    let graceful = tokio::select! {
        result = child.wait() => result.is_ok(),
        _ = tokio::time::sleep(KILL_GRACE) => false,
    };
    if graceful {
        return Ok(true);
    }

    // Force phase: SIGKILL the group, then reap.
    let _ = kill_group(pgid, Signal::SIGKILL);
    let _ = child.wait().await;
    Ok(false)
}

/// Force-kill (SIGKILL) an entire process group synchronously, for use where
/// async kill isn't available (e.g. `Drop`). `ESRCH` (already gone) is ignored.
///
/// Takes a raw `i32` (not `Pid`) so the caller — `supervisor`'s `Drop`, which
/// stores only a `u32` pid — stays decoupled from `nix`.
pub(super) fn force_kill_group(pgid: i32) {
    let _ = kill_group(Pid::from_raw(pgid), Signal::SIGKILL);
}

/// Send `sig` to every process in the process group `pgid`. `ESRCH` (no such
/// process) is treated as success — the group already exited.
fn kill_group(pgid: Pid, sig: Signal) -> Result<(), ShellError> {
    match killpg(pgid, Some(sig)) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => Ok(()), // already gone
        Err(e) => Err(ShellError::pgroup_kill_failed(pgid.as_raw(), e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kill_tree_reaps_orphaned_child() {
        // `sleep 30 &` backgrounds a child; without a group kill it would
        // survive the leader's death. process_group(0) + killpg reaps it.
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 30 & wait")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn");
        let pgid = Pid::from_raw(child.id().unwrap() as i32);

        kill_tree(&mut child).await.unwrap();

        // The whole group (leader + the backgrounded sleep) must be gone.
        // killpg(pgid, None) == signal 0; it returns ESRCH when no process
        // remains in the group. Scoped to this group — no cross-test interference.
        let result = killpg(pgid, None);
        assert_eq!(
            result,
            Err(Errno::ESRCH),
            "process group still has living members"
        );
    }
}
