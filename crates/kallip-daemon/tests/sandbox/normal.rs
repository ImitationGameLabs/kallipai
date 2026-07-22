//! Scenario 2 -- Normal root agent.
//!
//! Workspace and held dirlocks writable; `/tmp` baseline-writable; daemon data
//! tree read-only (write denied, read ok); skills carve writable; `.ssh` and
//! `profiles.toml` readable (Normal has no hide-holes -- secret protection is
//! Guest-side; this asserts the real semantics).

use std::path::Path;

use super::harness::*;

#[tokio::test]
#[serial_test::serial]
async fn scenario2_normal() {
    if unsupported() {
        return;
    }
    let world = World::setup();
    let ws = world.workspace.path().to_path_buf();
    let agent_data = "$KALLIP_DATA_DIR/agents/$KALLIP_ID";
    let script = vec![
        Reply::Tool(format!("echo hello > {}/test.txt", ws.display())), // 0: workspace writable
        Reply::Tool("kallip dirlock acquire $HOME/writable_subdir".into()), // 1: lock home subdir
        Reply::Tool("echo y > $HOME/writable_subdir/x".into()),         // 2: now writable
        Reply::Tool("echo t > /tmp/scenario2_tmp".into()),              // 3: /tmp baseline-writable
        Reply::Tool(format!("echo x >> {agent_data}/meta.json")),       // 4: data tree RO
        Reply::Tool(format!("cat {agent_data}/meta.json")),             // 5: read ok
        Reply::Tool(format!("echo s > {agent_data}/skills/s.md")),      // 6: skills carve
        Reply::Tool("ls -A $HOME/.ssh".into()),                         // 7: Normal reads .ssh
        Reply::Tool("cat $HOME/.ssh/id_testkey".into()),                // 8: contents readable
        Reply::Tool("cat $HOME/.config/kallip/profiles.toml".into()),   // 9: Normal reads profiles
        Reply::End("done"),
    ];

    let fx = start(world, &script, None).await;
    let run = run_agent(&fx.daemon).await;
    let meta_before =
        std::fs::read_to_string(agent_meta_path(&fx.data_root, &run.agent_id)).unwrap();

    let records = history_records(&fx.data_root, &run.agent_id);
    let results = bash_results(&records);

    assert_eq!(run.exit, "success", "{}", fx.daemon.diagnostics());
    assert!(
        results.len() >= 10,
        "expected >=10 bash results, got {}",
        results.len()
    );

    expect(&results, 0, "workspace write", true);
    expect(&results, 1, "dirlock acquire home subdir", true);
    expect(&results, 2, "home subdir write after lock", true);
    expect(&results, 3, "/tmp write", true);
    expect(&results, 4, "data-tree write denied", false);
    expect(&results, 5, "data-tree read ok", true);
    expect(&results, 6, "skills carve write", true);
    expect(&results, 7, ".ssh ls", true);
    // The LLM picks bash_exec's `capture` mode, so read via `text()` (merged /
    // stdout / stderr, whichever the mode surfaced) and match with `contains`.
    assert!(
        results[7].text().trim().contains("id_testkey"),
        ".ssh should list id_testkey for Normal, got: {:?}",
        results[7].text()
    );
    expect(&results, 8, ".ssh read", true);
    assert!(
        results[8].text().contains(SECRET_KEY),
        "Normal can read the ssh key (no hide-hole); got: {:?}",
        results[8].text()
    );
    expect(&results, 9, "profiles read", true);

    // FS corroboration.
    assert!(
        ws.join("test.txt").exists(),
        "workspace test.txt must exist"
    );
    assert!(
        Path::new("/tmp/scenario2_tmp").exists(),
        "/tmp file must exist"
    );
    assert!(
        fx.data_root
            .join("agents")
            .join(&run.agent_id)
            .join("skills/s.md")
            .exists(),
        "skills carve file must exist"
    );
    let meta_after =
        std::fs::read_to_string(agent_meta_path(&fx.data_root, &run.agent_id)).unwrap();
    assert_eq!(
        meta_before, meta_after,
        "meta.json must be unchanged (data tree is read-only)"
    );

    // /tmp cleanup so the assertion is repeatable.
    let _ = std::fs::remove_file("/tmp/scenario2_tmp");

    fx.daemon.kill().await;
}
