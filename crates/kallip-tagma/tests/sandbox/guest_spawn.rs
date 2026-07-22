//! Scenario 4 -- Explicit permission-class downgrade on subagent spawn.
//!
//! A Normal-tier parent spawns a subagent with `--permission-class guest`. This
//! exercises the full new path end-to-end: CLI flag -> protocol field -> tagma
//! reference-monitor downgrade (`resolve_granted_class`) -> persistence of the
//! granted class to `meta.json`. The happy path must succeed and print the
//! child id. A bad spelling is rejected at the `kallip` CLI by the flag's
//! `value_parser` before the tagma is ever contacted.
//!
//! The Guest *filesystem* semantics themselves are unchanged landlock code
//! already proven by `guest.rs` (driven by `permissions_class`); this scenario
//! focuses on the new spawn/downgrade interface rather than re-proving the
//! sandbox baseline for the subagent.

use super::harness::*;

#[tokio::test]
#[serial_test::serial]
async fn scenario4_guest_spawn_downgrade() {
    if unsupported() {
        return;
    }
    let world = World::setup();
    let ws = world.workspace.path().to_path_buf();
    let script = vec![
        Reply::Tool(format!(
            // 0: spawn a guest subagent (mkdir first -- the workspace must exist
            // for canonicalize). Success prints the child agent id.
            "mkdir -p {0}/reviewer && \
             kallip subagent spawn --permission-class guest \
             --workspace-root {0}/reviewer --role reviewer --prompt noop",
            ws.display()
        )),
        Reply::Tool(
            // 1: a bad spelling must be rejected by the CLI value_parser (exit != 0),
            // never reaching the tagma. Do not create the workspace first so a
            // tagma-side path can't accidentally succeed.
            "kallip subagent spawn --permission-class admin --role x --prompt noop".to_string(),
        ),
        Reply::End("done"),
    ];

    let fx = start(world, &script, None).await;
    let run = run_agent(&fx.tagma).await;

    let records = history_records(&fx.data_root, &run.agent_id);
    let results = bash_results(&records);

    assert_eq!(run.exit, "success", "{}", fx.tagma.diagnostics());
    assert!(
        results.len() >= 2,
        "expected >=2 bash results, got {}",
        results.len()
    );

    expect(&results, 0, "guest subagent spawn", true);
    let child_id = results[0].text().trim().to_string();
    assert!(
        !child_id.is_empty(),
        "subagent spawn should print the child agent id, got: {:?}",
        results[0].text()
    );

    // The downgraded class is persisted on the child's meta.json -- the
    // reference monitor accepted the explicit Guest beneath a Normal parent.
    let child_meta = std::fs::read_to_string(
        fx.data_root
            .join("agents")
            .join(&child_id)
            .join("meta.json"),
    )
    .unwrap_or_else(|e| panic!("read child meta.json: {e}"));
    assert!(
        child_meta.contains("\"Guest\""),
        "child meta.json must record the granted Guest class, got: {child_meta}"
    );

    // CLI-side rejection of an invalid class (clap value_parser names the flag).
    expect(&results, 1, "invalid class rejected by CLI", false);
    let err = results[1].text();
    assert!(
        err.contains("--permission-class"),
        "invalid class should name the flag, got: {err}"
    );

    fx.tagma.kill().await;
}
