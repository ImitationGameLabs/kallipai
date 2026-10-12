//! Unit tests for the background supervisor: registry lifecycle, watch-loop
//! terminal paths, and tail/prompt matching.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::ShellError;

use super::tail::read_tail;

use super::*;

/// Recorded terminal event `(task_id, state, exit_code)` for the on_terminal tests.
type CapturedTerminal = Option<(String, TaskState, Option<i32>, Option<String>)>;

fn registry() -> BackgroundRegistry {
    BackgroundRegistry::new(
        OsString::from("bash"),
        10 * 1024 * 1024,
        HashMap::new(),
        None,
    )
}

/// The headline carve-exclusion test, exercised through the real
/// `BackgroundRegistry` (no landlock needed): a running background task
/// bumps `running_bg` and makes the gate's WRITE side refuse a carve; once
/// the task is killed and the watcher decs, the gate is writable again.
/// This is the only test that drives the READ-across-fork + inc/dec wiring
/// end-to-end, covering both the carve-exclusion claim and the inc_bg
/// ordering (inc happens-before the watcher's dec).
#[tokio::test]
async fn running_bg_blocks_carve_while_task_runs() {
    let gate = crate::gate::ExecGate::new();
    let mut reg = registry().with_exec_gate(gate.clone());
    let id = reg.spawn("sleep 30").await.unwrap();

    // The spawn incs the tally before returning; wait for it to land.
    for _ in 0..50 {
        if gate.running_bg() == 1 {
            break;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    assert_eq!(gate.running_bg(), 1, "running task should bump the tally");
    assert_eq!(
        gate.try_write().unwrap_err(),
        crate::gate::ExecGateFailure::BgTasksRunning(1),
        "a carve must be refused while a background task runs"
    );

    reg.kill(&id).await.unwrap();
    // The watcher's cancel branch decs the tally; wait for it to settle.
    for _ in 0..50 {
        if gate.running_bg() == 0 {
            break;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    assert_eq!(gate.running_bg(), 0, "tally returns to zero after kill");
    assert!(
        gate.try_write().is_ok(),
        "a carve must succeed once no background task runs"
    );
}

#[tokio::test]
async fn spawn_then_read_exited_task() {
    let mut reg = registry();
    let id = reg.spawn("echo hello").await.unwrap();
    // Wait for the task to exit and the watcher to notice.
    for _ in 0..50 {
        let out = reg.read(&id, 4096).unwrap();
        if out.state == TaskState::Exited {
            assert!(out.output.contains("hello"));
            assert_eq!(out.exit_code, Some(0));
            return;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("task did not exit in time");
}

/// Color-suppression env vars reach a background task too (the `COLOR_VARS`
/// const is shared with the foreground path): all four entries are applied
/// — `TERM`/`NO_COLOR`/`CLICOLOR` set, `LS_COLORS` emptied.
#[tokio::test]
async fn spawn_applies_color_vars() {
    let mut reg = registry();
    let id = reg
        .spawn("echo \"$TERM/$NO_COLOR/$CLICOLOR\"; test -z \"$LS_COLORS\" && echo empty")
        .await
        .unwrap();
    for _ in 0..50 {
        let out = reg.read(&id, 4096).unwrap();
        if out.state == TaskState::Exited {
            assert_eq!(out.exit_code, Some(0));
            assert_eq!(out.output.trim(), "dumb/1/0\nempty");
            return;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("task did not exit in time");
}

#[tokio::test]
async fn kill_stops_a_long_task() {
    let mut reg = registry();
    let id = reg.spawn("sleep 30").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    reg.kill(&id).await.unwrap();
    let err = reg.read(&id, 4096).unwrap_err();
    assert!(matches!(err, ShellError::TaskNotFound { .. }));
}

#[tokio::test]
async fn read_unknown_task_errors() {
    let reg = registry();
    let err = reg.read("nope", 4096).unwrap_err();
    assert!(matches!(err, ShellError::TaskNotFound { .. }));
}

/// Pump-first cap path (the watchdog's twin): with the disk cap below
/// the default aligned watchdog threshold, the pump's pre-write check
/// fires first — the reason rides the read body and the task settles on
/// Exited (the cap final state), not the watchdog's Killed.
#[tokio::test]
async fn background_cap_stops_pump_and_surfaces_reason() {
    let mut reg = registry().with_disk_cap(4096);
    let id = reg
        .spawn("for i in {1..1000000}; do echo hello; done")
        .await
        .unwrap();
    for _ in 0..100 {
        let out = reg.read(&id, 1024).unwrap();
        if out.output.contains("disk cap") {
            assert!(
                out.output.contains("written to disk"),
                "reason carries the disk volume: {}",
                out.output
            );
            assert!(
                out.output.contains("narrow the command or redirect"),
                "recovery guidance rides the reason: {}",
                out.output
            );
            for _ in 0..50 {
                if reg.read(&id, 1024).unwrap().state != TaskState::Running {
                    break;
                }
                tokio::time::sleep(WATCH_POLL).await;
            }
            assert_eq!(
                reg.read(&id, 1024).unwrap().state,
                TaskState::Exited,
                "cap is the Exited final state, not watchdog Killed"
            );
            return;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("disk-cap reason never surfaced on the background read path");
}

/// The read-path bound: a windowed read against a file that is growing
/// while it is read must stay bounded — bytes returned within the
/// window and bounded latency. An unbounded read (plain read_to_end)
/// fails this test: it keeps draining the appends until the writer
/// stops.
#[tokio::test]
async fn read_tail_never_drains_a_growing_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("out.log");
    std::fs::write(&path, b"x").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = stop.clone();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&writer_path)
            .unwrap();
        let chunk = [b'a'; 256 * 1024];
        while !writer_stop.load(Ordering::Relaxed) {
            let _ = std::io::Write::write_all(&mut f, &chunk);
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    for _ in 0..10 {
        let out = read_tail(&path, 4096).unwrap();
        assert!(out.len() <= 4096, "window respected: {}", out.len());
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let _ = writer.join();
    assert!(
        elapsed < Duration::from_millis(500),
        "windowed reads must not drain the growing file: {elapsed:?}"
    );
}

#[tokio::test]
async fn size_watchdog_kills_overflow() {
    let mut reg = BackgroundRegistry::new(
        OsString::from("bash"),
        4096, // tiny cap
        HashMap::new(),
        None,
    );
    let id = reg
        .spawn("for i in {1..1000000}; do echo hello; done")
        .await
        .unwrap();
    for _ in 0..100 {
        let out = reg.read(&id, 1024).unwrap();
        if out.state == TaskState::Killed {
            return;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("size watchdog did not kill the overflowing task");
}

/// Build a registry whose terminal-state observer records the last
/// `(task_id, state, exit_code, reason)` into the returned shared slot.
fn tracking_registry(
    max_bg_bytes: usize,
) -> (BackgroundRegistry, Arc<std::sync::Mutex<CapturedTerminal>>) {
    use std::sync::Mutex;
    let received: Arc<Mutex<CapturedTerminal>> = Arc::new(Mutex::new(None));
    let captured = received.clone();
    let reg = BackgroundRegistry::new(
        OsString::from("bash"),
        max_bg_bytes,
        HashMap::new(),
        Some(Arc::new(move |id, state, code, reason| {
            *captured.lock().unwrap() =
                Some((id.to_string(), state, code, reason.map(str::to_owned)));
        })),
    );
    (reg, received)
}

#[tokio::test]
async fn on_terminal_fires_on_exit() {
    let (mut reg, received) = tracking_registry(10 * 1024 * 1024);
    let id = reg.spawn("exit 7").await.unwrap();
    for _ in 0..50 {
        if received.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    let (cb_id, cb_state, cb_code, cb_reason) = received
        .lock()
        .unwrap()
        .clone()
        .expect("on_terminal did not fire on exit");
    assert_eq!(cb_id, id);
    assert_eq!(cb_state, TaskState::Exited);
    assert_eq!(cb_code, Some(7));
    assert_eq!(cb_reason, None, "a plain exit carries no reason");
}

/// The finish face carries the termination story: a pump-capped task
/// fires `Exited` with the disk-cap reason (distinct from the watchdog's
/// reason-less `Killed`), and the read surface agrees — the reason field
/// is `Some` and the in-file truncation marker rides the tail window.
/// A kill afterwards must not reclaim the spill: out.log stays on disk.
#[tokio::test]
async fn terminal_notice_and_read_carry_cap_reason() {
    let (reg, received) = tracking_registry(10 * 1024 * 1024);
    // Tiny disk cap so the pump's pre-write check fires long before the
    // (never-reached) size watchdog: ~2.2 MB of bounded output vs 4 KiB.
    let mut reg = reg.with_disk_cap(4096);
    let id = reg
        .spawn("for i in {1..200000}; do echo line-$i; done")
        .await
        .unwrap();
    for _ in 0..100 {
        if received.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    let (cb_id, cb_state, _cb_code, cb_reason) = received
        .lock()
        .unwrap()
        .clone()
        .expect("on_terminal did not fire on cap");
    assert_eq!(cb_id, id);
    assert_eq!(
        cb_state,
        TaskState::Exited,
        "cap is the Exited+reason state"
    );
    let reason = cb_reason.expect("capped task fires with a reason");
    assert!(reason.contains("disk cap"), "{reason}");

    let out = reg.read(&id, 4096).unwrap();
    assert_eq!(out.reason.as_deref(), Some(reason.as_str()));
    assert!(
        out.output
            .contains("[output truncated: task disk cap reached]"),
        "in-file marker must ride the tail window: {}",
        out.output
    );

    // Regression pin: kill must not reclaim the spill — out.log stays
    // on disk after kill (the system /tmp policy owns cleanup). An
    // in-process cleanup that reclaims the file at kill fails this
    // assertion.
    reg.kill(&id).await.unwrap();
    let log = crate::spill::spill_root()
        .join("bg")
        .join(&id)
        .join("out.log");
    assert!(log.exists(), "out.log must survive kill: {}", log.display());
}

#[tokio::test]
async fn on_terminal_fires_on_size_watchdog() {
    // tiny cap → the size watchdog kills quickly.
    let (mut reg, received) = tracking_registry(4096);
    let id = reg
        .spawn("for i in {1..1000000}; do echo hello; done")
        .await
        .unwrap();
    for _ in 0..100 {
        if received.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    let (cb_id, cb_state, _cb_code, cb_reason) = received
        .lock()
        .unwrap()
        .clone()
        .expect("on_terminal did not fire on size-watchdog kill");
    assert_eq!(cb_reason, None, "a watchdog kill carries no pump reason");
    assert_eq!(cb_id, id);
    assert_eq!(cb_state, TaskState::Killed);
}
