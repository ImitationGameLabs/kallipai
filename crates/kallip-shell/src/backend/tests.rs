use super::*;
use crate::builder::ShellBuilder;

/// Collect all `bash_exec-*.txt` spill files directly under `root` (the spill
/// layout is flat -- no per-backend subdir).
fn spill_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("bash_exec-"))
        })
        .collect()
}

#[tokio::test]
async fn exec_captures_stdout_and_exit_code() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo hello; exit 7",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(7));
    assert!(out.output.as_deref().unwrap().contains("hello"));
    assert!(out.stdout.is_none() && out.stderr.is_none());
    assert!(!out.timed_out);
}

#[tokio::test]
async fn exec_cd_persists_across_calls() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let target = std::env::temp_dir();
    let cd = format!("cd '{}'", target.display());
    backend
        .exec(&cd, Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap();
    let out = backend
        .exec("pwd", Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap();
    // cwd is read fresh from the private fd channel after the cd -> sticky.
    assert_eq!(
        out.cwd,
        std::fs::canonicalize(&target)
            .unwrap()
            .display()
            .to_string()
    );
    assert!(out.output.as_deref().unwrap().trim() == out.cwd);
    // The cwd marker rides a separate fd, not the output stream, so the
    // merged text is pure command output.
    assert!(!out.output.as_deref().unwrap().contains("__ja_pwd"));
}

#[tokio::test]
async fn exec_timeout_converts_to_background_task() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "sleep 43 & wait",
            Duration::from_millis(400),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert!(out.timed_out);
    assert_eq!(
        out.exit_code, None,
        "no synthesized code: the command is still running"
    );
    let id = out.task_id.expect("converted to a background task");
    // The converted task reads like any background task.
    let read = backend.read_background(&id, 4096).await.unwrap();
    assert_eq!(read.state, supervisor::TaskState::Running);
    // Killing it reaps the whole process group.
    backend.kill_background(&id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pgrep = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("sleep 43")
        .output()
        .unwrap();
    assert!(
        pgrep.stdout.is_empty(),
        "orphaned `sleep 43` survived: {:?}",
        String::from_utf8_lossy(&pgrep.stdout)
    );
}

/// Exit-code fidelity: the cwd-marker read end moves into the task and
/// outlives bash, so the task's eventual exit status is the command's
/// real one (a dropped read end would SIGPIPE bash at its EXIT trap
/// and the code would be lost).
#[tokio::test]
async fn converted_task_reports_real_exit_code() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "sleep 0.3; exit 7",
            Duration::from_millis(100),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read = backend.read_background(&id, 4096).await.unwrap();
        if read.state == supervisor::TaskState::Exited {
            assert_eq!(read.exit_code, Some(7));
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "task never exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn converted_task_output_keeps_growing() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo one; sleep 0.5; echo two; sleep 30",
            Duration::from_millis(200),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    assert!(out.output.as_deref().unwrap().contains("one"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read = backend.read_background(&id, 4096).await.unwrap();
        if read.output.contains("two") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "second line never appeared: {:?}",
            read.output
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The sticky cwd does NOT advance on a conversion: the command has
/// not finished, so the pre-command value stays (unlike the old kill
/// path, where the trap fired during the graceful kill).
#[tokio::test]
async fn exec_timeout_conversion_does_not_advance_cwd() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let before = backend.cwd().to_path_buf();
    let target = std::env::temp_dir();
    let cmd = format!("cd '{}'; sleep 30", target.display());
    let out = backend
        .exec(&cmd, Duration::from_millis(400), CaptureMode::Merged)
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    assert_eq!(
        out.cwd,
        before.display().to_string(),
        "conversion freezes the sticky cwd"
    );
    assert_eq!(backend.cwd(), before);
    backend.kill_background(&id).await.unwrap();
}

#[tokio::test]
async fn zero_timeout_converts_immediately() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec("sleep 30", Duration::ZERO, CaptureMode::Merged)
        .await
        .unwrap();
    assert!(out.timed_out);
    let id = out.task_id.expect("immediate conversion");
    backend.kill_background(&id).await.unwrap();
}

/// A converted Separate-mode task joins its two captured streams
/// around a `[stderr]` divider on read (the pipes' true interleave
/// never existed).
#[tokio::test]
async fn converted_separate_task_joins_streams_with_divider() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo out-line; echo err-line 1>&2; sleep 30",
            Duration::from_millis(400),
            CaptureMode::Separate,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    assert!(out.stdout.as_deref().unwrap().contains("out-line"));
    assert!(out.stderr.as_deref().unwrap().contains("err-line"));
    let read = backend.read_background(&id, 4096).await.unwrap();
    assert!(
        read.output.contains("[stderr]"),
        "divider joins the streams: {:?}",
        read.output
    );
    backend.kill_background(&id).await.unwrap();
}

/// Dropping the `exec` future before its own timeout (the runtime's cancel
/// / tool-timeout path) must kill the whole process group, not just the
/// leader. The backend timeout (30s) outlasts the outer drop (500ms), so
/// the cancel path is exercised, not the backend's `kill_tree` timeout.
#[tokio::test]
async fn exec_cancel_kills_process_group_no_orphans() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let outer = tokio::time::timeout(
        Duration::from_millis(500),
        backend.exec(
            "sleep 44 & wait",
            Duration::from_secs(30),
            CaptureMode::Merged,
        ),
    )
    .await;
    // The outer timeout must fire (cancel path), not the backend's 30s one.
    assert!(outer.is_err(), "outer timeout should have fired, not exec");
    // The orphaned group is reaped asynchronously after the SIGKILL, so poll
    // for it to be gone rather than asserting instantaneously (follows the
    // polling shape of `pgroup::tests::kill_tree_reaps_orphaned_child`).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let pgrep = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("sleep 44")
            .output()
            .unwrap();
        if pgrep.stdout.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "orphaned `sleep 44` survived cancel: {}",
            String::from_utf8_lossy(&pgrep.stdout)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Gate battery for the conversion path: a converted task bumps
/// running_bg for its lifetime (carve refused while it runs), the
/// tally drains on kill, and the kill reaps the whole group.
#[tokio::test]
async fn converted_task_blocks_carve_until_killed() {
    let gate = crate::gate::ExecGate::new();
    let mut backend = ShellBuilder::new()
        .exec_gate(gate.clone())
        .build()
        .await
        .unwrap();
    let out = backend
        .exec(
            "sleep 45 & wait",
            Duration::from_millis(400),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    for _ in 0..50 {
        if gate.running_bg() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gate.running_bg(), 1, "converted task bumps the tally");
    assert_eq!(
        gate.try_write().unwrap_err(),
        crate::gate::ExecGateFailure::BgTasksRunning(1),
        "a carve must be refused while the converted task runs"
    );
    backend.kill_background(&id).await.unwrap();
    for _ in 0..50 {
        if gate.running_bg() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gate.running_bg(), 0, "kill drains the tally");
    assert!(gate.try_write().is_ok(), "carve allowed after the kill");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pgrep = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("sleep 45")
        .output()
        .unwrap();
    assert!(
        pgrep.stdout.is_empty(),
        "orphaned `sleep 45` survived: {:?}",
        String::from_utf8_lossy(&pgrep.stdout)
    );
}

/// The tally also drains when the converted task exits on its own, and
/// the exited entry stays readable with its exit code.
#[tokio::test]
async fn converted_task_tally_drains_when_it_exits() {
    let gate = crate::gate::ExecGate::new();
    let mut backend = ShellBuilder::new()
        .exec_gate(gate.clone())
        .build()
        .await
        .unwrap();
    let out = backend
        .exec("sleep 0.3", Duration::from_millis(100), CaptureMode::Merged)
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read = backend.read_background(&id, 4096).await.unwrap();
        if read.state == supervisor::TaskState::Exited {
            assert_eq!(read.exit_code, Some(0));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "task never exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for _ in 0..50 {
        if gate.running_bg() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        gate.running_bg(),
        0,
        "tally drains when the converted task exits"
    );
    assert!(gate.try_write().is_ok());
}

/// The registry Drop settles the tally for a still-running converted
/// task and force-kills its group.
#[tokio::test]
async fn converted_task_tally_drains_on_registry_drop() {
    let gate = crate::gate::ExecGate::new();
    let mut backend = ShellBuilder::new()
        .exec_gate(gate.clone())
        .build()
        .await
        .unwrap();
    let out = backend
        .exec("sleep 46", Duration::from_millis(400), CaptureMode::Merged)
        .await
        .unwrap();
    assert!(out.task_id.is_some());
    for _ in 0..50 {
        if gate.running_bg() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gate.running_bg(), 1);
    drop(backend);
    for _ in 0..50 {
        if gate.running_bg() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(gate.running_bg(), 0, "registry Drop drains the tally");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pgrep = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("sleep 46")
        .output()
        .unwrap();
    assert!(
        pgrep.stdout.is_empty(),
        "orphaned `sleep 46` survived the registry Drop: {:?}",
        String::from_utf8_lossy(&pgrep.stdout)
    );
}

/// Carve-epoch close: a carve landing between the exec's fork and the
/// timeout conversion refuses the adoption — the child is killed
/// (today's timeout behavior) and the envelope says so instead of
/// returning a task id.
#[tokio::test]
async fn carve_landing_after_fork_refuses_the_conversion() {
    let gate = crate::gate::ExecGate::new();
    let mut backend = ShellBuilder::new()
        .exec_gate(gate.clone())
        .build()
        .await
        .unwrap();
    // Repeatedly attempt the WRITE side; it succeeds once the exec's
    // fork releases the READ (long before its timeout).
    let carver_gate = gate.clone();
    let carver = tokio::spawn(async move {
        loop {
            if let Ok(guard) = carver_gate.try_write() {
                drop(guard);
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let out = backend
        .exec(
            "cd /tmp && sleep 30",
            Duration::from_millis(500),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    carver.await.unwrap();
    assert!(out.timed_out);
    assert!(
        out.task_id.is_none(),
        "the conversion must be refused after a carve landed"
    );
    let text = out.output.as_deref().unwrap_or("");
    assert!(
        text.contains("could not be converted"),
        "envelope explains the refusal: {text}"
    );
    // The refusal runs the old kill semantics, which still probe the
    // cwd via the SIGTERM trap: the post-command directory is reported.
    assert_eq!(out.cwd, "/tmp");
    // Nothing was registered, so the tally stays at zero.
    assert_eq!(gate.running_bg(), 0);
}

/// Overflow before the conversion: the spill banner rides the
/// conversion envelope, the spill file lives while the task runs, and
/// killing the task unlinks it.
#[tokio::test]
async fn overflow_before_conversion_keeps_banner_and_cleans_spill() {
    let spill = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .max_output_bytes(2048)
        .spill_dir(spill.path())
        .build()
        .await
        .unwrap();
    // 8KiB of output overflows the 2KiB budget and spills; the command
    // then outlives the timeout and is converted.
    let out = backend
        .exec(
            "head -c 8192 /dev/zero | tr '\\0' 'a'; sleep 30",
            Duration::from_millis(500),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    let text = out.output.as_deref().unwrap();
    assert!(
        text.contains("was clipped"),
        "banner present in the conversion envelope: {text}"
    );
    assert!(
        text.contains("cat "),
        "recovery affordance names the spill file: {text}"
    );
    assert_eq!(spill_files(spill.path()).len(), 1, "spill file exists");
    backend.kill_background(&id).await.unwrap();
    assert!(
        spill_files(spill.path()).is_empty(),
        "spill file unlinked when the task is killed"
    );
}

/// The Pipes size watchdog: an adopted (converted) task whose captured
/// output total crosses max_bg_bytes is killed — the File-path twin is
/// size_watchdog_kills_overflow; this pins the adopted-task branch.
#[tokio::test]
async fn converted_pipes_size_watchdog_kills_overflow() {
    let mut backend = ShellBuilder::new()
        .max_output_bytes(2048)
        .max_bg_bytes(8192)
        .build()
        .await
        .unwrap();
    let out = backend
        .exec(
            "head -c 65536 /dev/zero | tr '\\0' 'a'; sleep 30",
            Duration::from_millis(400),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let read = backend.read_background(&id, 4096).await.unwrap();
        if read.state == supervisor::TaskState::Killed {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "watchdog never killed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// drain_pumps recovery: a grandchild the command backgrounded keeps the
/// pipe write-end open after the child exits; the drain deadline aborts
/// the stuck pump, the watcher still reaches Exited with the real code,
/// and the surviving output is readable (drain never consumes captures).
#[tokio::test]
async fn converted_grandchild_pipe_drain_reaches_terminal() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo grandchild-pipe; sleep 5 & sleep 1; exit 0",
            Duration::from_millis(400),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let read = backend.read_background(&id, 4096).await.unwrap();
        if read.state == supervisor::TaskState::Exited {
            assert_eq!(read.exit_code, Some(0));
            assert!(
                read.output.contains("grandchild-pipe"),
                "output survived the drain: {}",
                read.output
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "drain never finished"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The grandchild (sleep 5) outlives the task and dies naturally;
    // killing stragglers after the leader exits is not this path's
    // contract (the drain deadline, 1s < 5s, is what unblocks the
    // watcher).
}

/// C-I4: after a converted task exits, the terminal drain must not
/// consume the captures — bg_read still returns the retained output
/// (clipped head+tail view) with the real exit code.
#[tokio::test]
async fn converted_task_read_after_exit_keeps_head_and_tail() {
    let mut backend = ShellBuilder::new()
        .max_output_bytes(2048)
        .build()
        .await
        .unwrap();
    let out = backend
        .exec(
            "head -c 8192 /dev/zero | tr '\\0' 'b'; sleep 0.8; exit 3",
            Duration::from_millis(300),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let id = out.task_id.expect("converted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let read = backend.read_background(&id, 4096).await.unwrap();
        if read.state == supervisor::TaskState::Exited {
            assert_eq!(read.exit_code, Some(3));
            assert!(read.output.contains('b'), "head retained");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "task never exited");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let read = backend.read_background(&id, 4096).await.unwrap();
    assert_eq!(read.state, supervisor::TaskState::Exited);
    assert!(
        read.output.contains('b'),
        "second read still has the output"
    );
}

/// Registry Drop path: a spill file created after adoption is unlinked
/// when the backend (and its registry) is dropped, not only on kill.
#[tokio::test]
async fn dropping_backend_unlinks_post_adoption_spill() {
    let spill = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .max_output_bytes(2048)
        .spill_dir(spill.path())
        .build()
        .await
        .unwrap();
    // Output continues after the conversion so the capture spills a
    // second time post-adoption.
    let out = backend
            .exec(
                "(head -c 2048 /dev/zero | tr '\\0' 'c'; sleep 0.6; head -c 8192 /dev/zero | tr '\\0' 'c'); sleep 30",
                Duration::from_millis(300),
                CaptureMode::Merged,
            )
            .await
            .unwrap();
    let id = out.task_id.expect("converted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while spill_files(spill.path()).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "capture never spilled post-adoption"
        );
        let _ = backend.read_background(&id, 4096).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(backend);
    assert!(
        spill_files(spill.path()).is_empty(),
        "registry Drop must unlink the post-adoption spill"
    );
}

/// A non-zero exit still fires the EXIT trap, so the sticky cwd roundtrip
/// reports the post-command directory.
#[tokio::test]
async fn exit_n_traps_and_reports_cwd() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let dir_a = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let tmp_b = tempfile::TempDir::new().unwrap();
    let dir_b = std::fs::canonicalize(tmp_b.path()).unwrap();
    backend
        .exec(
            &format!("cd '{}'", dir_a.display()),
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let out = backend
        .exec(
            &format!("cd '{}' ; exit 42", dir_b.display()),
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(42));
    assert_eq!(out.cwd, dir_b.display().to_string());
    // Sticky cwd persists to the next call.
    let out = backend
        .exec("pwd", Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap();
    assert_eq!(out.cwd, dir_b.display().to_string());
}

/// If a command removes its own cwd, the marker's payload targets a gone
/// directory and `cwd::resolve_str`'s canonicalize guard falls back rather
/// than reporting a stale path.
#[tokio::test]
async fn deleted_cwd_falls_back() {
    let tmp = tempfile::TempDir::new().unwrap();
    let doomed = tmp.path().join("doomed");
    std::fs::create_dir_all(&doomed).unwrap();
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            &format!("cd '{}' && rmdir '{}'", doomed.display(), doomed.display()),
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert!(
        std::path::Path::new(&out.cwd).exists(),
        "cwd should fall back to an existing dir, not the deleted one; got {}",
        out.cwd
    );
}

/// Under `CaptureMode::Merged` the script prepends `exec 2>&1`, so fd 2
/// points at the stdout pipe. The cwd marker rides a separate fd channel
/// (independent of fd 2), so a command that also does its own `exec 2>&1`
/// still recovers cwd and the merged stream carries only command output.
#[tokio::test]
async fn exec_with_merged_stderr_recovers_cwd() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = std::fs::canonicalize(tmp.path()).unwrap();
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            &format!("cd '{}' && exec 2>&1 && echo merged", target.display()),
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(
        out.cwd,
        target.display().to_string(),
        "cwd must still recover even when the command merges its own streams"
    );
    // Only `merged` is populated; stdout/stderr are None under Merged.
    let merged = out.output.as_deref().unwrap();
    assert!(out.stdout.is_none() && out.stderr.is_none());
    // The command's own output survives; no marker bytes leak.
    assert!(merged.contains("merged"), "merged: {merged}");
    assert!(!merged.contains("__ja_pwd"));
}

/// Color-suppression env vars reach the spawned bash via `Command::env`
/// (the script no longer emits them): all four `COLOR_VARS` entries are
/// applied — `TERM`/`NO_COLOR`/`CLICOLOR` set, `LS_COLORS` emptied.
#[tokio::test]
async fn color_vars_suppress_in_foreground() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo \"$TERM/$NO_COLOR/$CLICOLOR\"; test -z \"$LS_COLORS\" && echo empty",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.output.as_deref().unwrap().trim(), "dumb/1/0\nempty");
}

/// Foreground `exec` writes nothing under the spawn cwd, and an under-budget
/// exec writes nothing under `spill_dir` either: the script rides argv, the
/// cwd rides the fd channel, and a spill file appears only on overflow.
#[tokio::test]
async fn exec_leaves_no_scratch_in_cwd() {
    let probe = std::env::temp_dir().join(format!(
        "ja-shell-probe-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&probe).unwrap();
    let scratch = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .initial_cwd(probe.clone())
        .spill_dir(scratch.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let out = backend
        .exec("echo hi", Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap();
    // Nothing written under the spawn cwd.
    let mut entries = std::fs::read_dir(&probe).unwrap();
    assert!(
        entries.next().is_none(),
        "foreground exec left files behind in {}",
        probe.display()
    );
    // Under budget: no spill file, no clip, no marker.
    assert!(!out.truncated);
    assert!(!out.output.as_deref().unwrap().contains("bytes omitted"));
    assert!(
        spill_files(scratch.path()).is_empty(),
        "under-budget exec wrote a spill file"
    );
    let _ = std::fs::remove_dir_all(&probe);
}

/// A command whose `bash -c` script exceeds `MAX_SCRIPT_BYTES` is rejected
/// up front with an actionable error, before any spawn is attempted (so no
/// partial side effects and no process is started).
#[tokio::test]
async fn exec_rejects_oversized_command() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    // 9000 bytes of payload -> script well over the 8 KiB cap.
    let oversized = format!("printf '{}'", "x".repeat(9000));
    let err = backend
        .exec(&oversized, Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ShellError::CommandTooLarge { limit } if limit == MAX_SCRIPT_BYTES
    ));
}

/// A var set via the builder reaches the command (process inheritance +
/// `Command::env` replace the removed snapshot).
#[tokio::test]
async fn builder_env_reaches_exec() {
    let mut backend = ShellBuilder::new()
        .env("JA_INHERIT_PROBE", "ok")
        .build()
        .await
        .unwrap();
    let out = backend
        .exec(
            "echo \"${JA_INHERIT_PROBE:?unset}\"",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.output.as_deref().unwrap().trim(), "ok");
}

// -- CaptureMode coverage -------------------------------------------------

/// `Merged` (the default) interleaves stdout and stderr into one stream via
/// the `exec 2>&1` prepended to the script: both `out` and `err` reach the
/// single `merged` field; `stdout`/`stderr` stay `None`.
#[tokio::test]
async fn exec_merged_interleaves_streams() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo out; echo err >&2",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let merged = out.output.as_deref().unwrap();
    assert!(merged.contains("out"), "merged: {merged}");
    assert!(merged.contains("err"), "merged: {merged}");
    assert!(out.stdout.is_none() && out.stderr.is_none());
    assert!(!out.truncated);
}

/// `Separate` keeps the two streams apart: stdout has `out` (not `err`),
/// stderr has `err` (not `out`), `merged` is `None`.
#[tokio::test]
async fn exec_separate_keeps_streams_apart() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            "echo out; echo err >&2",
            Duration::from_secs(10),
            CaptureMode::Separate,
        )
        .await
        .unwrap();
    let stdout = out.stdout.as_deref().unwrap();
    let stderr = out.stderr.as_deref().unwrap();
    assert!(stdout.contains("out") && !stdout.contains("err"));
    assert!(stderr.contains("err") && !stderr.contains("out"));
    assert!(out.output.is_none());
}

/// `Stdout` returns only stdout but still recovers the cwd: the marker rides
/// the private fd channel, so it does not depend on capturing stderr. The
/// returned `stderr` is `None`.
#[tokio::test]
async fn exec_stdout_mode_recovers_cwd() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = std::fs::canonicalize(tmp.path()).unwrap();
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            &format!("cd '{}' && echo hi", target.display()),
            Duration::from_secs(10),
            CaptureMode::Stdout,
        )
        .await
        .unwrap();
    assert_eq!(out.stdout.as_deref().unwrap().trim(), "hi");
    assert!(out.stderr.is_none() && out.output.is_none());
    assert_eq!(
        out.cwd,
        target.display().to_string(),
        "Stdout mode must still recover cwd via the fd channel"
    );
}

/// `Stderr` mode under a command that redirects fd 2 onto stdout
/// (`exec 2>&1`): the redirect points fd 2 at the stdout pipe, so the stderr
/// capture sees nothing and the returned `stderr` is empty. The cwd is still
/// recovered -- the marker rides the fd channel, independent of fd 2.
#[tokio::test]
async fn exec_stderr_mode_with_command_merge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = std::fs::canonicalize(tmp.path()).unwrap();
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec(
            &format!("cd '{}' && exec 2>&1 && echo hi", target.display()),
            Duration::from_secs(10),
            CaptureMode::Stderr,
        )
        .await
        .unwrap();
    // The command's `exec 2>&1` pointed fd 2 at fd 1, so the stderr capture
    // saw nothing: stderr is empty.
    assert_eq!(out.stderr.as_deref().unwrap(), "");
    assert!(out.stdout.is_none() && out.output.is_none());
    assert_eq!(out.cwd, target.display().to_string());
}

/// `Merged` overflow clips the single combined capture to a head+tail view,
/// flags `truncated`, prepends the recovery banner, and spills the complete
/// stream to a file whose contents equal the full emitted output.
#[tokio::test]
async fn exec_merged_truncation_single_stream() {
    let scratch = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .max_output_bytes(64)
        .spill_dir(scratch.path().to_path_buf())
        .build()
        .await
        .unwrap();
    // Write well over the 64-byte budget; in Merged both fds land in one
    // capture, which clips to head+tail. The cwd marker rides the fd
    // channel, so it does not pollute the captured stream.
    let out = backend
        .exec(
            "printf 'A%.0s' {1..200}; printf 'B%.0s' {1..200} >&2",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert!(out.truncated, "merged stream should be clipped");
    let merged = out.output.as_deref().unwrap();
    assert!(
        merged.contains("bytes omitted"),
        "head+tail view has the middle-omitted marker: {merged}"
    );
    assert!(
        merged.contains("was clipped (middle omitted)"),
        "banner prepended: {merged}"
    );
    assert!(
        merged.contains("with: cat"),
        "banner carries the cat hint: {merged}"
    );
    // No marker bytes from the fd channel leak into the captured stream.
    assert!(!merged.contains("__ja_pwd"));
    // Exactly one spill file, holding the complete stream.
    let files = spill_files(scratch.path());
    assert_eq!(files.len(), 1, "only one spill file under Merged");
    let spilled = std::fs::read(&files[0]).unwrap();
    // 200 'A's + 200 'B's = the complete emitted output, in some order.
    assert_eq!(spilled.len(), 400);
    assert_eq!(spilled.iter().filter(|&&b| b == b'A').count(), 200);
    assert_eq!(spilled.iter().filter(|&&b| b == b'B').count(), 200);
}

/// `Separate` overflow spills each stream to its own file and banners each
/// clipped stream; the non-clipped stream is clean (no banner).
#[tokio::test]
async fn exec_separate_overflow_spills_each_stream() {
    let scratch = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .max_output_bytes(64)
        .spill_dir(scratch.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let out = backend
        .exec(
            "printf 'A%.0s' {1..200}; printf 'B%.0s' {1..200} >&2",
            Duration::from_secs(10),
            CaptureMode::Separate,
        )
        .await
        .unwrap();
    assert!(out.truncated);
    let stdout = out.stdout.as_deref().unwrap();
    let stderr = out.stderr.as_deref().unwrap();
    assert!(stdout.contains("clipped (middle omitted)"));
    assert!(stderr.contains("clipped (middle omitted)"));
    // Two distinct spill files (-stdout / -stderr).
    let spills: Vec<String> = spill_files(scratch.path())
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        spills.len(),
        2,
        "two spill files under Separate: {spills:?}"
    );
    assert!(spills.iter().any(|n| n.ends_with("-stdout.txt")));
    assert!(spills.iter().any(|n| n.ends_with("-stderr.txt")));
}

// -- CwdProbe / script-shape / spill-security tests -----------------------

/// The trap script redirects to the bare-integer fd (`>&63`), not the
/// `>&{63}` brace form (which bash treats as a filename and silently
/// no-ops). This shape test would have caught that regression.
#[test]
fn build_exec_script_uses_bare_fd_redirect() {
    let s = build_exec_script("cmd", Some(63), CaptureMode::Merged);
    assert!(
        s.contains("pwd -P >&63"),
        "trap must use the bare-integer fd redirect, got: {s}"
    );
    assert!(!s.contains(">&{63}"), "brace form is a silent no-op: {s}");
}

/// With no marker fd the script omits the trap entirely (pipe setup failed).
#[test]
fn build_exec_script_omits_trap_when_no_fd() {
    let s = build_exec_script("cmd", None, CaptureMode::Merged);
    assert!(!s.contains("__ja_pwd"));
}

/// `CwdProbe` round-trips a pwd written to the write end: writing a path
/// line, dropping the write end, then reading yields the trimmed pwd.
#[test]
fn cwd_probe_reads_pwd_from_fd_channel() {
    let (probe, write_end) = CwdProbe::new().unwrap();
    // Write the way the trap would (a path + newline) via a borrowed fd
    // (no ownership transfer), then drop the write end so the read end EOFs.
    let _ = nix::unistd::write(write_end.0.as_fd(), b"/srv/example\n");
    drop(write_end);
    let pwd = probe.read_cwd().unwrap();
    assert_eq!(pwd, "/srv/example");
}

/// `read_cwd` does not hang when a write-end copy stays open (a stand-in for
/// a backgrounded grandchild inheriting the marker fd): the read is
/// nonblocking and returns whatever the trap wrote, then stops at EAGAIN.
#[tokio::test]
async fn cwd_probe_does_not_hang_when_write_end_stays_open() {
    let (probe, write_end) = CwdProbe::new().unwrap();
    let _ = nix::unistd::write(write_end.0.as_fd(), b"/srv/example\n");
    // Do NOT drop write_end -- emulate a grandchild holding the fd open.
    let pwd = tokio::time::timeout(Duration::from_secs(2), async { probe.read_cwd() })
        .await
        .expect("read_cwd must not hang on a held write end");
    assert_eq!(pwd.unwrap(), "/srv/example");
}

/// A probe whose write end is dropped without writing yields no cwd (the
/// trap never fired / SIGKILL before EXIT).
#[test]
fn cwd_probe_empty_when_trap_never_fired() {
    let (probe, write_end) = CwdProbe::new().unwrap();
    drop(write_end);
    assert!(probe.read_cwd().is_none());
}

/// The marker fd is at or above `MARKER_FD_FLOOR`.
#[test]
fn cwd_probe_marker_fd_is_high() {
    let (_probe, write_end) = CwdProbe::new().unwrap();
    assert!(write_end.fd() >= MARKER_FD_FLOOR);
}

/// A spilled file is created owner-only (0o600): no info leak to other uids
/// on a multi-user host.
#[tokio::test]
async fn spill_file_is_owner_only() {
    let scratch = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .max_output_bytes(32)
        .spill_dir(scratch.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let out = backend
        .exec(
            "printf 'A%.0s' {1..200}",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    assert!(out.truncated);
    let spill = spill_files(scratch.path()).pop().expect("a spill file");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&spill).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "spill file must be owner-only, got {:o}", mode);
}

/// The overflow-time spill open refuses a symlinked `spill_dir`: even if an
/// adversary swaps the dir for a symlink between build and the first overflow,
/// the `O_NOFOLLOW` dir open poisons (no banner, no spill path) and the write
/// does NOT follow the symlink into its target. Guards the TOCTOU.
#[tokio::test]
async fn spill_refuses_symlinked_spill_dir() {
    let root = tempfile::TempDir::new().unwrap();
    let dir = root.path().join("dir");
    let target = root.path().join("target");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    // Build against the real dir (passes the build-time check).
    let mut backend = ShellBuilder::new()
        .max_output_bytes(32)
        .spill_dir(dir.clone())
        .build()
        .await
        .unwrap();
    // Swap: replace the real dir with a symlink -> target, then overflow.
    std::fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&target, &dir).unwrap();
    let out = backend
        .exec(
            "printf 'A%.0s' {1..200}",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    // Overflow happened but the spill poisoned: no recovery banner.
    assert!(out.truncated, "stream should still be clipped in-memory");
    assert!(
        !out.output
            .as_deref()
            .unwrap()
            .contains("read the full output with"),
        "no banner when spill_dir is a symlink at overflow time"
    );
    // The write did NOT follow the symlink into the target dir.
    assert!(
        std::fs::read_dir(&target).unwrap().next().is_none(),
        "spill wrote through the symlinked spill_dir into the target"
    );
}

/// A SIGKILL before the EXIT trap fires loses the cwd (no trap ran); the
/// caller falls back rather than reporting a stale path.
#[tokio::test]
async fn sigkill_before_trap_falls_back() {
    let mut backend = ShellBuilder::new().build().await.unwrap();
    let out = backend
        .exec("kill -9 $$", Duration::from_secs(10), CaptureMode::Merged)
        .await
        .unwrap();
    assert!(
        std::path::Path::new(&out.cwd).exists(),
        "cwd must fall back to an existing dir"
    );
}

/// A second landlocked `bash` can `cat` a spill file the tagma parent
/// wrote under `temp_dir()`: `baseline_writable` grants read on writable
/// paths. Guards the read-back affordance the banner advertises.
#[cfg(all(target_os = "linux", feature = "landlock"))]
#[tokio::test]
async fn spilled_file_is_readable_by_landlocked_cat() {
    use crate::landlock;
    if landlock::ensure_supported().is_err() {
        return;
    }
    let scratch = tempfile::TempDir::new().unwrap();
    let mut backend = ShellBuilder::new()
        .max_output_bytes(32)
        .spill_dir(scratch.path().to_path_buf())
        .access_source(|| {
            Ok(landlock::AccessDecision {
                read: landlock::ReadPolicy::Broad,
                writable: Vec::new(),
                readonly_holes: Vec::new(),
                hide_holes: Vec::new(),
            })
        })
        .build()
        .await
        .unwrap();
    let first = backend
        .exec(
            "printf 'HEAD'; printf 'A%.0s' {1..200}",
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let merged = first.output.as_deref().unwrap();
    let path = merged
        .lines()
        .next()
        .and_then(|line| {
            line.split("with: cat ")
                .nth(1)
                .and_then(|rest| rest.trim_end_matches(']').trim().to_string().into())
        })
        .expect("banner with a spill path");
    let second = backend
        .exec(
            &format!("cat '{path}'"),
            Duration::from_secs(10),
            CaptureMode::Merged,
        )
        .await
        .unwrap();
    let reread = second.output.as_deref().unwrap();
    assert!(
        reread.contains("HEAD"),
        "landlocked cat read the spill back"
    );
}
