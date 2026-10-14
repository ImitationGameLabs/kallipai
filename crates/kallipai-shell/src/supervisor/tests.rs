//! Unit tests for the background supervisor: registry lifecycle, watch-loop
//! terminal paths, and tail/prompt matching.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::ShellError;

use super::registry::MAX_COMMAND_CHARS;
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
        None,
    )
}

/// A registry with a running-task sink installed, plus that sink — the shape
/// a host uses to read the list without touching the backend lock.
fn registry_with_sink() -> (BackgroundRegistry, BgTaskSink) {
    let sink: BgTaskSink = Arc::new(std::sync::Mutex::new(Vec::new()));
    let reg = BackgroundRegistry::new(
        OsString::from("bash"),
        10 * 1024 * 1024,
        HashMap::new(),
        None,
        Some(sink.clone()),
    );
    (reg, sink)
}

/// The cwd handed to a registry spawn under test: the child starts there and
/// the registry records it, so a real directory keeps the assertions
/// comparable to production values.
fn test_cwd() -> PathBuf {
    std::env::current_dir().unwrap()
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
    let id = reg.spawn("sleep 30", test_cwd()).await.unwrap();

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
    let id = reg.spawn("echo hello", test_cwd()).await.unwrap();
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
        .spawn(
            "echo \"$TERM/$NO_COLOR/$CLICOLOR\"; test -z \"$LS_COLORS\" && echo empty",
            test_cwd(),
        )
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
    let id = reg.spawn("sleep 30", test_cwd()).await.unwrap();
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
        .spawn("for i in {1..1000000}; do echo hello; done", test_cwd())
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
        None,
    );
    let id = reg
        .spawn("for i in {1..1000000}; do echo hello; done", test_cwd())
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
        None,
    );
    (reg, received)
}

#[tokio::test]
async fn on_terminal_fires_on_exit() {
    let (mut reg, received) = tracking_registry(10 * 1024 * 1024);
    let id = reg.spawn("exit 7", test_cwd()).await.unwrap();
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
        .spawn("for i in {1..200000}; do echo line-$i; done", test_cwd())
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
        .spawn("for i in {1..1000000}; do echo hello; done", test_cwd())
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

/// The recorded trio reaches the summary: command text, cwd, and spawn time.
/// A multi-line command is collapsed to a single line — newlines and tabs
/// become single spaces — so no consumer has to defend its layout against a
/// heredoc.
#[tokio::test]
async fn handles_record_command_cwd_and_started_at() {
    let mut reg = registry();
    let cwd = std::env::temp_dir();
    let before = epoch_now();
    let id = reg
        .spawn("sleep 30\n\t&& echo a\t\tb", cwd.clone())
        .await
        .unwrap();
    let handles = reg.handles();
    assert_eq!(handles.len(), 1, "the one running task is summarized");
    let summary = handles[0].summary();
    assert_eq!(summary.id, id);
    assert_eq!(
        summary.cwd, cwd,
        "the recorded cwd is what the caller passed"
    );
    assert_eq!(
        summary.command, "sleep 30 && echo a b",
        "whitespace runs collapse to single spaces"
    );
    assert!(
        summary.started_at >= before && summary.started_at <= epoch_now(),
        "spawn time is bracketed by the test's own clock"
    );
    assert!(!summary.stalled, "a silent task is not stalled");
    assert_eq!(summary.bytes, 0, "nothing has been written yet");
    reg.kill(&id).await.unwrap();
}

/// Only running tasks are summarized: once a task is terminal it leaves the
/// list instead of being reported as finished — so there is no terminal
/// group, and the list cannot grow into a history.
#[tokio::test]
async fn handles_drop_a_task_once_it_is_terminal() {
    let mut reg = registry();
    let id = reg.spawn("echo done", test_cwd()).await.unwrap();
    for _ in 0..50 {
        if reg.read(&id, 4096).unwrap().state == TaskState::Exited {
            assert!(reg.handles().is_empty(), "an exited task is not listed");
            return;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("task did not exit in time");
}

/// Newest first, capped at the snapshot cap: with more running tasks than the
/// cap, the list holds the newest and drops the oldest. The oldest task is
/// spawned a second earlier so the drop is unambiguous — same-second spawns
/// order on the id, which is random.
#[tokio::test]
async fn handles_are_newest_first_and_capped() {
    let mut reg = registry();
    let oldest = reg.spawn("sleep 30", test_cwd()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut newer = Vec::new();
    for _ in 0..MAX_BG_SUMMARIES {
        newer.push(reg.spawn("sleep 30", test_cwd()).await.unwrap());
    }
    let handles = reg.handles();
    assert_eq!(
        handles.len(),
        MAX_BG_SUMMARIES,
        "capped at the snapshot cap"
    );
    assert!(
        !handles.iter().any(|s| s.id == oldest),
        "the oldest task is the one dropped"
    );
    assert!(
        newer.iter().all(|id| handles.iter().any(|s| &s.id == id)),
        "every newer task is listed"
    );
    for pair in handles.windows(2) {
        assert!(
            pair[0].started_at >= pair[1].started_at,
            "newest first: {pair:?}"
        );
    }
    for id in &newer {
        reg.kill(id).await.unwrap();
    }
    reg.kill(&oldest).await.unwrap();
}

/// The command cap counts characters, not bytes, and a cut is marked: a
/// multibyte tail keeps its characters whole (a byte-wise cut would split
/// one), and the marker tells the reader the text is short of the original.
/// The cap is applied once, at the source, so consumers never re-cap.
#[tokio::test]
async fn command_is_capped_by_characters_and_marked() {
    let mut reg = registry();
    let prefix = "sleep 30; echo ";
    let id = reg
        .spawn(&format!("{prefix}{}", "é".repeat(300)), test_cwd())
        .await
        .unwrap();
    let handles = reg.handles();
    assert_eq!(handles.len(), 1);
    let command = &handles[0].command;
    assert_eq!(
        command.chars().count(),
        MAX_COMMAND_CHARS + 1,
        "the cap plus the cut marker"
    );
    assert!(command.ends_with('…'), "a cut is marked: {command}");
    assert_eq!(
        command.matches('é').count(),
        MAX_COMMAND_CHARS - prefix.chars().count(),
        "multibyte characters survive whole"
    );
    assert!(command.starts_with(prefix), "the head is kept: {command}");
    reg.kill(&id).await.unwrap();
}
/// The cap is compared against the whole command: one that fits exactly is
/// passed through untouched, and the first character over the cap is what
/// earns the marker.
#[tokio::test]
async fn command_length_is_compared_against_the_cap_exactly() {
    let mut reg = registry();
    let prefix = "sleep 30; # ";
    let at_cap = format!(
        "{prefix}{}",
        "x".repeat(MAX_COMMAND_CHARS - prefix.chars().count())
    );
    let over_cap = format!("{at_cap}y");
    let first = reg.spawn(&at_cap, test_cwd()).await.unwrap();
    let second = reg.spawn(&over_cap, test_cwd()).await.unwrap();
    let mut commands: Vec<String> = reg.handles().into_iter().map(|s| s.command).collect();
    commands.sort();
    let mut expected = vec![at_cap.clone(), format!("{at_cap}…")];
    expected.sort();
    assert_eq!(
        commands, expected,
        "at the cap: verbatim; one over: cut and marked"
    );
    reg.kill(&first).await.unwrap();
    reg.kill(&second).await.unwrap();
}

/// Wall clock in epoch seconds, for bracketing a recorded spawn time.
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The sink follows the running set: a spawn publishes the task, a kill
/// removes it, and a task that exits on its own leaves at its terminal
/// transition — the list is the running set, not a history.
#[tokio::test]
async fn sink_publishes_the_running_set() {
    let (mut reg, sink) = registry_with_sink();
    let first = reg.spawn("sleep 30", test_cwd()).await.unwrap();
    let second = reg.spawn("echo done", test_cwd()).await.unwrap();
    assert_eq!(
        published(&sink).len(),
        2,
        "both running tasks are published"
    );
    reg.kill(&first).await.unwrap();
    let after_kill = published(&sink);
    assert_eq!(after_kill.len(), 1, "a killed task leaves the list");
    assert_eq!(after_kill[0].id, second);
    for _ in 0..50 {
        if reg.read(&second, 4096).unwrap().state == TaskState::Exited {
            assert!(
                published(&sink).is_empty(),
                "an exited task leaves the list"
            );
            return;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("task did not exit in time");
}

/// A published entry's byte count follows the task itself: the cell is
/// replaced only when the running set changes, so a reader must see the bytes
/// written since that publish rather than the count frozen at publish time.
/// The command writes, pauses past a watcher poll, then writes again — the
/// running set does not change in between.
#[tokio::test]
async fn published_bytes_follow_a_running_task() {
    let (mut reg, sink) = registry_with_sink();
    let id = reg
        .spawn("printf aaaa; sleep 2; printf bbbb; sleep 30", test_cwd())
        .await
        .unwrap();
    let first = await_bytes(&sink, |bytes| bytes > 0).await;
    let second = await_bytes(&sink, |bytes| bytes > first).await;
    assert!(
        second > first,
        "the count grows without another publish: {first} then {second}"
    );
    assert_eq!(published(&sink).len(), 1, "the running set did not change");
    assert_eq!(published(&sink)[0].id, id);
    reg.kill(&id).await.unwrap();
}

/// Poll the published cell for an entry whose byte count satisfies `ok`.
async fn await_bytes(sink: &BgTaskSink, ok: impl Fn(usize) -> bool) -> usize {
    for _ in 0..100 {
        if let Some(bytes) = published(sink).first().map(|task| task.summary().bytes)
            && ok(bytes)
        {
            return bytes;
        }
        tokio::time::sleep(WATCH_POLL).await;
    }
    panic!("the published byte count never satisfied the test");
}

/// Dropping the registry empties the sink: the tasks go away with the agent's
/// runtime, so a reader must not keep seeing the last frame as running.
#[tokio::test]
async fn sink_is_empty_after_the_registry_is_dropped() {
    let (mut reg, sink) = registry_with_sink();
    let _id = reg.spawn("sleep 30", test_cwd()).await.unwrap();
    assert_eq!(published(&sink).len(), 1);
    drop(reg);
    assert!(
        published(&sink).is_empty(),
        "the last frame is cleared with the registry"
    );
}

/// The sink's contents, for assertions.
fn published(sink: &BgTaskSink) -> Vec<BgTaskHandle> {
    sink.lock().unwrap().clone()
}
