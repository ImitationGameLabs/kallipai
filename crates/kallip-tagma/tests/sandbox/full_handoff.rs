//! Scenario 5 -- Full-handoff workspace delegation.
//!
//! A `--full-handoff` child takes the supervisor's *entire* workspace
//! write-lock for its lifetime. Exercises the real spawn-carve-remove
//! lifecycle under landlock:
//!   - while the child lives the supervisor cannot write its own workspace
//!     (the lock was transferred to the child);
//!   - a second subagent is refused (full-handoff exclusivity);
//!   - after the child is removed the lock returns to the supervisor and the
//!     workspace is writable again.
//!
//! This is the only test that drives the full `establish_workspace_lock` carve
//! (forward transfer + acquire) and the `remove_agent` transfer-back through
//! the real HTTP/CLI path, complementing the in-process unit test of
//! `establish_workspace_lock`'s drop-order invariant.

use super::harness::*;

#[tokio::test]
#[serial_test::serial]
async fn scenario5_full_handoff() {
    if unsupported() {
        return;
    }
    let world = World::setup();
    let ws = world.workspace.path().to_path_buf();
    // The spawned child id is captured to /tmp (baseline-writable, persists
    // across the separate shell of each bash_exec) so the remove step can read
    // it. The workspace itself is unwritable once the handoff fires, so the id
    // cannot live there.
    let child_id_file =
        std::env::temp_dir().join(format!("kallip-fh-child-{}", std::process::id()));
    let script = vec![
        // 0: write own workspace (before handoff).
        Reply::Tool(format!("echo root > {}/own.txt", ws.display())),
        // 1: spawn a full-handoff child on the SAME workspace (required: identity).
        Reply::Tool(format!(
            "kallip subagent spawn --workspace-root {} --full-handoff --role worker > {}",
            ws.display(),
            child_id_file.display()
        )),
        // 2: supervisor write to its own workspace must now FAIL (lock transferred).
        Reply::Tool(format!("echo blocked > {}/after.txt", ws.display())),
        // 3: a second subagent must be refused (full-handoff exclusivity).
        Reply::Tool(format!(
            "kallip subagent spawn --workspace-root {} --full-handoff --role other",
            ws.display()
        )),
        // 4: remove the child -- the lock transfers back to the supervisor.
        Reply::Tool(format!(
            "kallip subagent remove \"$(cat {})\"",
            child_id_file.display()
        )),
        // 5: supervisor write succeeds again (lock regained).
        Reply::Tool(format!("echo regained > {}/back.txt", ws.display())),
        Reply::End("done"),
    ];

    let fx = start(world, &script, None).await;
    let run = run_agent(&fx.tagma).await;

    let records = history_records(&fx.data_root, &run.agent_id);
    let results = bash_results(&records);

    assert_eq!(run.exit, "success", "{}", fx.tagma.diagnostics());
    assert!(
        results.len() >= 6,
        "expected >=6 bash results, got {}; {}",
        results.len(),
        fx.tagma.diagnostics()
    );

    expect(
        &results,
        0,
        "supervisor own workspace write (pre-handoff)",
        true,
    );
    expect(&results, 1, "full-handoff child spawn", true);
    expect(
        &results,
        2,
        "supervisor workspace readonly while child holds the lock",
        false,
    );
    expect(
        &results,
        3,
        "second spawn refused (full-handoff exclusivity)",
        false,
    );
    let exclusivity_msg = results[3].text();
    assert!(
        exclusivity_msg.contains("full-handoff"),
        "second spawn should cite full-handoff exclusivity, got: {:?}",
        exclusivity_msg
    );
    expect(&results, 4, "remove full-handoff child", true);
    expect(
        &results,
        5,
        "supervisor workspace writable after child removal",
        true,
    );

    // FS corroboration.
    assert!(ws.join("own.txt").exists(), "own.txt must exist");
    assert!(
        !ws.join("after.txt").exists(),
        "after.txt must not exist -- the supervisor was blocked from its workspace while the child lived"
    );
    assert!(ws.join("back.txt").exists(), "back.txt must exist");

    fx.tagma.kill().await;
}
