//! Watcher loop for one background task: poll to its terminal state, drain the
//! pumps, and drive the stall and size watchdogs.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;

use crate::capture::{self, BoundedCapture};
use crate::pgroup;

use super::registry::{Pumps, TaskOutput, WatchArgs, Watched, mark_terminal};
use super::tail::{lock_cap, tail_matches};
use super::{
    EXIT_NONE, OnTaskTerminal, STALL_QUIESCENCE, STATE_EXITED, STATE_KILLED, TaskState, WATCH_POLL,
};

/// Write one pumped chunk to a background task's out.log under the
/// task-wide disk cap: the chunk is appended only if the total stays
/// within the cap, so out.log is strictly bounded — the on-disk size can
/// never overshoot by a partial chunk. Returns `false` when the pump must
/// stop: cap reached, or the log write failed (poison degrade — the
/// failure is surfaced, never silently swallowed). The caller then closes
/// the read ends; the child's next write dies on SIGPIPE or fails with
/// EPIPE, and the reason reaches read/finish via the shared slot; a single
/// truncation marker is appended to out.log so tail-window reads carry the
/// cut point in-file. A pathological child that ignores SIGPIPE and never
/// exits stays Running with zero further inflow; only a manual kill()
/// terminates it.
pub(crate) async fn pump_log_chunk(
    log: &mut tokio::fs::File,
    quota: &capture::DiskQuota,
    cap_reason: &Arc<Mutex<Option<String>>>,
    chunk: &[u8],
) -> bool {
    if !quota.try_reserve(chunk.len()) {
        let reason = capture::disk_cap_reason(quota);
        *cap_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
        // Single truncation marker at EOF: tail-window reads carry the cut
        // point in-file, while the reason (read body prefix / `reason`
        // field / terminal notice) carries the recovery guidance. The file
        // stays pure bytes + marker, readable with head/tail as-is.
        // Best-effort: the cap reason is already set, so a failed marker
        // write must not overwrite it with a poison story.
        let _ = log
            .write_all(b"\n[output truncated: task disk cap reached]\n")
            .await;
        return false;
    }
    if log.write_all(chunk).await.is_err() {
        // Poison degrade: hand the reservation back and surface the
        // failure — an unbounded task never hides behind a dead log.
        quota.release(chunk.len());
        *cap_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some("out.log write failed; captured output degraded (poison)".to_owned());
        return false;
    }
    true
}
/// Watcher loop: detect exit, run the size + stall watchdogs, and drive a
/// two-phase kill on cancel. Branches per output source ([`TaskOutput`]):
/// spawned tasks stat their log file, adopted tasks total their live
/// captures — the watchdog semantics are identical either way.
pub(crate) async fn watch(args: WatchArgs) {
    let WatchArgs {
        mut child,
        watched,
        cancel,
        max_bg_bytes,
        id,
        on_terminal,
        exec_gate,
        output,
        cap_reason,
        mut pumps,
        marker_read,
    } = args;
    // Hold the adopted task's marker read end for this watcher's whole
    // life (see WatchArgs::marker_read); every `return` below drops it,
    // always after the child is terminal.
    let _marker_keepalive = marker_read;
    let Watched {
        state,
        exit_code,
        stalled,
        bytes,
    } = watched;
    let mut last_size: usize = 0;
    let mut quiescent_since: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = pgroup::kill_tree(&mut child).await;
                drain_pumps(&mut pumps).await;
                mark_terminal(&state, STATE_KILLED, exec_gate.as_ref());
                let reason = task_reasons(&output, cap_reason.as_ref()).into_iter().next();
                fire_terminal(&on_terminal, &id, TaskState::Killed, None, reason.as_deref());
                return;
            }
            _ = tokio::time::sleep(WATCH_POLL) => {}
        }

        let size = match &output {
            TaskOutput::File { path } => {
                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as usize
            }
            TaskOutput::Pipes { out, err, .. } => pipes_total(out, err.as_ref()),
        };
        bytes.store(size, Ordering::Relaxed);

        // Size watchdog: unbounded output fills the disk (the 768GB lesson).
        if size > max_bg_bytes {
            let _ = pgroup::kill_tree(&mut child).await;
            drain_pumps(&mut pumps).await;
            mark_terminal(&state, STATE_KILLED, exec_gate.as_ref());
            let reason = task_reasons(&output, cap_reason.as_ref())
                .into_iter()
                .next();
            fire_terminal(
                &on_terminal,
                &id,
                TaskState::Killed,
                None,
                reason.as_deref(),
            );
            return;
        }

        // Exit detection.
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                exit_code.store(code.unwrap_or(EXIT_NONE), Ordering::Relaxed);
                // Drain the adopted task's pumps (if any) WITHOUT consuming
                // the captures — post-exit reads must still see every byte.
                drain_pumps(&mut pumps).await;
                mark_terminal(&state, STATE_EXITED, exec_gate.as_ref());
                let reason = task_reasons(&output, cap_reason.as_ref())
                    .into_iter()
                    .next();
                fire_terminal(
                    &on_terminal,
                    &id,
                    TaskState::Exited,
                    code,
                    reason.as_deref(),
                );
                return;
            }
            Ok(None) => {}
            Err(_) => {
                drain_pumps(&mut pumps).await;
                mark_terminal(&state, STATE_EXITED, exec_gate.as_ref());
                let reason = task_reasons(&output, cap_reason.as_ref())
                    .into_iter()
                    .next();
                fire_terminal(
                    &on_terminal,
                    &id,
                    TaskState::Exited,
                    None,
                    reason.as_deref(),
                );
                return;
            }
        }

        // Stall watchdog: requires quiescence, then a tail-regex match.
        if size == last_size {
            let since = quiescent_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= STALL_QUIESCENCE && tail_matches(&output) {
                stalled.store(true, Ordering::Relaxed);
            }
        } else {
            quiescent_since = None;
            stalled.store(false, Ordering::Relaxed);
        }
        last_size = size;
    }
}

/// Drain an adopted task's pipe pumps at terminal state, WITHOUT consuming
/// the captures (post-exit reads must still see the full output — taking
/// and finishing a capture here would empty the task). Mirrors the
/// foreground path: once the child dies the pipes close and the pumps see
/// EOF promptly; the bound only binds when a grandchild the command
/// backgrounded still holds a write-end.
///
/// Kill-path time budget: kill_tree (<=2s SIGTERM grace + SIGKILL reap)
/// plus at most 2x1s here must stay under KILL_JOIN's 5s, so a `kill()`
/// caller never waits past its own join bound. Keep these constants in
/// sync if any is retuned.
async fn drain_pumps(pumps: &mut Option<Pumps>) {
    let Some(p) = pumps.take() else { return };
    for mut handle in [Some(p.out), p.err].into_iter().flatten() {
        if tokio::time::timeout(crate::backend::PUMP_DRAIN_DEADLINE, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await; // resolves promptly with Cancelled after abort
        }
    }
}

/// Total bytes seen across an adopted task's captured streams. Monotonic
/// even after clipping freezes the rendered view (the head freezes and the
/// tail rolls while `total` keeps counting), so both watchdogs and the
/// `bytes` counter keep moving while a clipped command keeps printing.
fn pipes_total(
    out: &Arc<Mutex<BoundedCapture>>,
    err: Option<&Arc<Mutex<BoundedCapture>>>,
) -> usize {
    lock_cap(out).total_bytes() + err.map(|e| lock_cap(e).total_bytes()).unwrap_or(0)
}

/// Invoke the terminal-state observer, if registered. A panic in the callback
/// propagates through the watcher task (surfaced by tokio's default handler).
fn fire_terminal(
    on_terminal: &Option<OnTaskTerminal>,
    id: &str,
    state: TaskState,
    exit_code: Option<i32>,
    reason: Option<&str>,
) {
    if let Some(cb) = on_terminal {
        cb(id, state, exit_code, reason);
    }
}

/// Reasons worth surfacing for a task, in read-body order: the pump/cap
/// slot first, then the adopted captures' peeks. Empty when the task ended
/// with no cap/poison story (a plain exit, or a watchdog kill whose story
/// the `Killed` state already tells). Shared by `read` (body prefix +
/// `reason` field) and the terminal notice (finish face), so all three
/// surfaces carry identical wording from the same source.
pub(crate) fn task_reasons(
    output: &TaskOutput,
    cap_reason: Option<&Arc<Mutex<Option<String>>>>,
) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    if let Some(r) = cap_reason.and_then(|slot| slot.lock().ok().and_then(|g| g.clone())) {
        reasons.push(r);
    }
    if let TaskOutput::Pipes { out, err, .. } = output {
        for cap in Some(out).into_iter().chain(err.as_ref()) {
            if let Some(r) = lock_cap(cap).peek().killed_reason {
                reasons.push(r);
            }
        }
    }
    reasons
}
