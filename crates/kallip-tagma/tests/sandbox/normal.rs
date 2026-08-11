//! Scenario 2 -- Normal root agent.
//!
//! Workspace and held dirlocks writable; `/tmp` baseline-writable and
//! `/dev/shm` Normal-extra writable; tagma data tree read-only (write denied,
//! read ok); `.ssh` and `profiles.toml` readable
//! (Normal has no hide-holes -- secret protection is Guest-side; this asserts
//! the real semantics).

use std::path::Path;

use super::harness::*;

#[tokio::test]
#[serial_test::serial]
async fn scenario2_normal() {
    if unsupported() {
        return;
    }
    // `/dev/shm` is absent on some restricted containers; guard so the shm
    // assertions no-op there rather than fail. `is_dir()` (not `exists()`) so a
    // hostile regular file at that path is not written to.
    let have_shm = Path::new("/dev/shm").is_dir();
    let world = World::setup();
    let ws = world.workspace.path().to_path_buf();
    let agent_data = "$KALLIP_DATA_DIR/agents/$KALLIP_ID";
    let mut script = vec![
        Reply::Tool(format!("echo hello > {}/test.txt", ws.display())), // 0: workspace writable
        Reply::Tool("kallip dirlock acquire $HOME/writable_subdir".into()), // 1: lock home subdir
        Reply::Tool("echo y > $HOME/writable_subdir/x".into()),         // 2: now writable
        Reply::Tool("echo t > /tmp/scenario2_tmp".into()),              // 3: /tmp baseline-writable
        Reply::Tool(format!("echo x >> {agent_data}/meta.json")),       // 4: data tree RO
        Reply::Tool(format!("cat {agent_data}/meta.json")),             // 5: read ok
        Reply::Tool("ls -A $HOME/.ssh".into()),                         // 6: Normal reads .ssh
        Reply::Tool("cat $HOME/.ssh/id_testkey".into()),                // 7: contents readable
        Reply::Tool("cat $HOME/.config/kallip/profiles.toml".into()),   // 8: Normal reads profiles
    ];
    if have_shm {
        script.push(Reply::Tool("echo s > /dev/shm/scenario2_shm".into())); // 9: /dev/shm writable
    }
    script.push(Reply::End("done"));

    let fx = start(world, &script, None).await;
    let run = run_agent(&fx.tagma).await;
    let meta_before =
        std::fs::read_to_string(agent_meta_path(&fx.data_root, &run.agent_id)).unwrap();

    let records = history_records(&fx.data_root, &run.agent_id);
    let results = bash_results(&records);

    assert_eq!(run.exit, "success", "{}", fx.tagma.diagnostics());
    assert!(
        results.len() >= 9,
        "expected >=9 bash results, got {}",
        results.len()
    );

    expect(&results, 0, "workspace write", true);
    expect(&results, 1, "dirlock acquire home subdir", true);
    expect(&results, 2, "home subdir write after lock", true);
    expect(&results, 3, "/tmp write", true);
    expect(&results, 4, "data-tree write denied", false);
    expect(&results, 5, "data-tree read ok", true);
    expect(&results, 6, ".ssh ls", true);
    // The LLM picks bash_exec's `capture` mode, so read via `text()` (merged /
    // stdout / stderr, whichever the mode surfaced) and match with `contains`.
    assert!(
        results[6].text().trim().contains("id_testkey"),
        ".ssh should list id_testkey for Normal, got: {:?}",
        results[6].text()
    );
    expect(&results, 7, ".ssh read", true);
    assert!(
        results[7].text().contains(SECRET_KEY),
        "Normal can read the ssh key (no hide-hole); got: {:?}",
        results[7].text()
    );
    expect(&results, 8, "profiles read", true);
    if have_shm {
        expect(&results, 9, "/dev/shm write", true);
    }

    // FS corroboration.
    assert!(
        ws.join("test.txt").exists(),
        "workspace test.txt must exist"
    );
    assert!(
        Path::new("/tmp/scenario2_tmp").exists(),
        "/tmp file must exist"
    );
    let meta_after =
        std::fs::read_to_string(agent_meta_path(&fx.data_root, &run.agent_id)).unwrap();
    assert_eq!(
        meta_before, meta_after,
        "meta.json must be unchanged (data tree is read-only)"
    );

    // /tmp cleanup so the assertion is repeatable.
    let _ = std::fs::remove_file("/tmp/scenario2_tmp");
    // /dev/shm corroboration + cleanup.
    if have_shm {
        assert!(
            Path::new("/dev/shm/scenario2_shm").exists(),
            "/dev/shm file must exist"
        );
        let _ = std::fs::remove_file("/dev/shm/scenario2_shm");
    }

    fx.tagma.kill().await;
}
