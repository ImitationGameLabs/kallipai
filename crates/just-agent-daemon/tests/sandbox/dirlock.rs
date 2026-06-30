//! Scenario 3 -- Subagent + nested dirlock + mutual exclusion.
//!
//! A child's nested workspace becomes a readonly hole to the parent via the
//! delegation carve, while the parent keeps writing its own workspace; a second
//! subagent locking an overlapping path is rejected (409).

use super::harness::*;

#[tokio::test]
#[serial_test::serial]
async fn scenario3_dirlock() {
    if unsupported() {
        return;
    }
    let world = World::setup();
    let ws = world.workspace.path().to_path_buf();
    let script = vec![
        Reply::Tool(format!("echo root > {}/own.txt", ws.display())), // 0: own workspace
        Reply::Tool(format!(
            // 1: spawn child (mkdir first -- the workspace must exist for canonicalize)
            "mkdir -p {0}/child && just-agent aide spawn --workspace-root {0}/child --role worker --prompt noop",
            ws.display()
        )),
        Reply::Tool(format!("echo p > {}/child/inside.txt", ws.display())), // 2: child WS RO to root
        Reply::Tool(format!("echo q > {}/own2.txt", ws.display())), // 3: own workspace writable
        Reply::Tool(format!(
            // 4: 409 conflict
            "just-agent aide spawn --workspace-root {}/child --role worker2 --prompt noop",
            ws.display()
        )),
        Reply::End("done"),
    ];

    let fx = start(world, &script, None).await;
    let run = run_agent(&fx.daemon, &ws, script.len() + 2).await;

    let records = history_records(&fx.data_root, &run.agent_id);
    let results = bash_results(&records);

    assert_eq!(run.exit, "success", "{}", fx.daemon.diagnostics());
    assert!(
        results.len() >= 5,
        "expected >=5 bash results, got {}",
        results.len()
    );

    expect(&results, 0, "root own workspace write", true);
    expect(&results, 1, "aide spawn child", true);
    // The child agent id is the aide-spawn stdout.
    let child_id = results[1].stdout.trim().to_string();
    assert!(
        !child_id.is_empty(),
        "aide spawn should print the child agent id, got: {:?}",
        results[1].stdout
    );
    expect(&results, 2, "child workspace readonly to root", false);
    expect(&results, 3, "root own workspace still writable", true);
    expect(&results, 4, "second spawn must conflict", false);
    let conflict_msg = format!("{}\n{}", results[4].stderr, results[4].stdout);
    assert!(
        conflict_msg.contains("overlaps") || conflict_msg.contains("held by agent"),
        "second spawn should report a conflict, got stderr={:?} stdout={:?}",
        results[4].stderr,
        results[4].stdout
    );

    // FS corroboration.
    assert!(ws.join("own.txt").exists(), "own.txt must exist");
    assert!(ws.join("own2.txt").exists(), "own2.txt must exist");
    assert!(
        !ws.join("child/inside.txt").exists(),
        "child workspace is readonly to root -- inside.txt must not exist"
    );

    // The child agent has history (it ran one no-op round against the child mock).
    let child_records = history_records(&fx.data_root, &child_id);
    assert!(
        !child_records.is_empty(),
        "child agent should have recorded history"
    );

    fx.daemon.kill().await;
}
