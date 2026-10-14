//! The tool dispatch hands the caller's running-set cell to the shell backend:
//! a background command run through the tools lands in that cell, which is what
//! the tagma's per-agent status surfaces read.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use kallipai_adk::config::AgentConfig;
use kallipai_adk::context::ContextStore;
use kallipai_adk::dirlock::DirLockManager;
use kallipai_adk::names::{BASH_EXEC, BG_KILL};
use kallipai_adk::persistence::{InstanceRoots, install_instance_roots};
use kallipai_adk::tools::{ToolDispatchInputs, build_tool_dispatch};
use kallipai_common::AgentId;
use kallipai_common::policy::ExecPolicy;
use kallipai_shell::BgTaskSink;
use tokio::sync::Mutex;

/// The process-wide instance roots, installed once for this test binary (the
/// production installer refuses a second call).
fn roots() -> &'static tempfile::TempDir {
    static ROOTS: OnceLock<tempfile::TempDir> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("roots tempdir");
        for name in ["data", "config", "state"] {
            std::fs::create_dir_all(dir.path().join(name)).expect("roots subdir");
        }
        install_instance_roots(InstanceRoots {
            data: dir.path().join("data"),
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        })
        .expect("install instance roots");
        dir
    })
}

#[tokio::test]
async fn background_tasks_land_in_the_callers_cell() {
    let roots = roots();
    let workspace = roots.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let config = AgentConfig {
        workspace_root: workspace,
        ..AgentConfig::default()
    };

    let bg_tasks: BgTaskSink = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dispatch = build_tool_dispatch(ToolDispatchInputs {
        ctx: Arc::new(Mutex::new(ContextStore::new())),
        config: &config,
        env: HashMap::new(),
        notice_sink: Arc::new(|_text| {}),
        exec_policy: Arc::new(RwLock::new(ExecPolicy::default())),
        lock_manager: Arc::new(DirLockManager::new()),
        agent_id: AgentId::from("dispatch-test".to_owned()),
        exec_gate: kallipai_shell::ExecGate::new(),
        bg_tasks: bg_tasks.clone(),
    })
    .await
    .expect("build the tool dispatch");

    let launched = dispatch
        .call_tool(BASH_EXEC, r#"{"command":"sleep 30","background":true}"#)
        .await
        .expect("launch a background command");
    let task_id =
        serde_json::from_str::<serde_json::Value>(&launched).expect("tool payload")["task_id"]
            .as_str()
            .expect("a launched task reports its id")
            .to_owned();

    let published = bg_tasks.lock().unwrap().clone();
    assert_eq!(published.len(), 1, "the launched task is published");
    assert_eq!(published[0].id, task_id);
    assert_eq!(published[0].command, "sleep 30");
    let agent_dir = std::fs::canonicalize(&config.workspace_root).unwrap();
    assert_ne!(
        agent_dir,
        std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
        "the test tells the agent directory from the process cwd"
    );
    assert_eq!(
        published[0].cwd, agent_dir,
        "the recorded cwd is the directory the task starts in"
    );

    let killed = dispatch
        .call_tool(BG_KILL, &format!(r#"{{"task_id":"{task_id}"}}"#))
        .await
        .expect("kill the background command");
    assert!(
        killed.contains(&task_id),
        "the kill names the task it stopped: {killed}"
    );
    for _ in 0..50 {
        if bg_tasks.lock().unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("a killed task stays out of the cell");
}
