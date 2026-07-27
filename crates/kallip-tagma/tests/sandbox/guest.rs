//! Scenario 1 -- Guest root agent.
//!
//! Secrets masked by empty tmpfs hide-holes; workspace, data tree, and home
//! writes denied (Guest is read-only).

use super::harness::*;

#[tokio::test]
#[serial_test::serial]
async fn scenario1_guest() {
    if unsupported() {
        return;
    }
    let world = World::setup();
    let ws = world.workspace.path().to_path_buf();
    let agent_data = "$KALLIP_DATA_DIR/agents/$KALLIP_ID";
    let script = vec![
        Reply::Tool("ls -A $HOME/.ssh".into()), // 0: hide-hole => empty
        Reply::Tool(format!("echo x > {}/probe.txt", ws.display())), // 1: workspace RO
        Reply::Tool(format!("echo x >> {agent_data}/meta.json")), // 2: data tree RO
        Reply::Tool("mkdir -p $HOME/elsewhere && echo x > $HOME/elsewhere/x".into()), // 3: home RO
        Reply::Tool("cat $HOME/.config/kallip/profiles.toml".into()), // 4: profiles hide-hole
        Reply::End("done"),
    ];

    let fx = start(world, &script, Some("guest")).await;
    let run = run_agent(&fx.tagma).await;
    let records = history_records(&fx.data_root, &run.agent_id);
    let results = bash_results(&records);

    assert_eq!(run.exit, "success", "{}", fx.tagma.diagnostics());
    assert!(
        results.len() >= 5,
        "expected >=5 bash results, got {}",
        results.len()
    );

    expect(&results, 0, "guest .ssh hidden (empty ls)", true);
    assert!(
        results[0].text().trim().is_empty(),
        "guest .ssh should be empty (tmpfs hide-hole), got: {:?}",
        results[0].text()
    );
    expect(&results, 1, "guest workspace write denied", false);
    expect(&results, 2, "guest data-tree write denied", false);
    expect(&results, 3, "guest home write denied", false);
    expect(&results, 4, "guest profiles read denied (hide-hole)", false);

    // FS corroboration.
    assert!(
        !ws.join("probe.txt").exists(),
        "workspace probe.txt must not exist"
    );
    // The host key is untouched (the tmpfs overlay is per-namespace).
    assert_eq!(
        std::fs::read_to_string(fx.world.home_path().join(".ssh/id_testkey")).unwrap(),
        SECRET_KEY
    );

    fx.tagma.kill().await;
}
