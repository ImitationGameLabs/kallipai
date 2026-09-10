//! End-to-end lifecycle: the daemon spawns a REAL kallip-tagma binary via
//! the detach helper, health reads the self-written pid/port, stop lands
//! SIGTERM and the process exits. The daemon resolves its helpers by
//! bare name (KALLIP_BIN_DIR → PATH): these tests hand the daemon a
//! KALLIP_BIN_DIR pinning the workspace build dir that the test-local
//! resolve_bin finds (KALLIP_BIN_DIR → CARGO_BIN_EXE_* → target-dir
//! parent → PATH) — green under `cargo test` and the dev container
//! alike. A package-scoped `cargo build -p kallip-daemon` does NOT
//! produce the workspace binaries: build the workspace first or
//! these tests spuriously fail.

use std::path::PathBuf;
use std::time::Duration;

use kallip_daemon_client::DaemonClient;
use kallip_daemon_common::wire::{ErrorCode, InstanceState, OkPayload, RequestBody, ResponseBody};

// The daemon crate is a binary; integration tests cannot import it. The
// lifecycle surface under test is the four verbs over the wire, so the
// tests boot the real daemon binary as a subprocess.
struct DaemonProc {
    socket: PathBuf,
    child: std::process::Child,
    _data: PathBuf,
    data_dir: tempfile::TempDir,
    records: PathBuf,
    log_path: PathBuf,
}

fn resolve_bin(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("KALLIP_BIN_DIR")
        && let p = std::path::Path::new(&dir).join(name)
        && p.is_file()
    {
        return p;
    }
    let var = format!("CARGO_BIN_EXE_{name}");
    if let Ok(p) = std::env::var(&var) {
        return PathBuf::from(p);
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(std::path::Path::to_path_buf))
        && exe_dir.ends_with("deps")
        && let Some(profile_dir) = exe_dir.parent()
    {
        let in_target = profile_dir.join(name);
        if in_target.is_file() {
            return in_target;
        }
    }
    PathBuf::from(name)
}

fn start_daemon() -> DaemonProc {
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let socket = state_dir.path().join("control.sock");
    let records = state_dir.path().join("kallipai/daemon/instances");
    let bin = resolve_bin("kallip-daemon");
    // stdout+stderr land in a file (not null) so tests can assert on
    // the daemon's own log — the quiet-Dead guarantee is a log claim.
    let log_path = state_dir.path().join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let mut child = std::process::Command::new(&bin)
        .env("XDG_DATA_HOME", data_dir.path())
        .env(
            "KALLIP_BIN_DIR",
            resolve_bin("kallip-tagma")
                .parent()
                // &Path has no Default; the empty path falls through to bare-name
                // PATH lookup.
                .unwrap_or(std::path::Path::new("")),
        )
        // The record root rides the default derivation from the state
        // home, exercising the production resolution path end to end.
        .env("XDG_STATE_HOME", state_dir.path())
        .env(
            "KALLIP_DAEMON_SOCKET",
            state_dir.path().join("control.sock"),
        )
        .stdout(log.try_clone().expect("clone log handle"))
        .stderr(log)
        .spawn()
        .expect("spawn daemon");
    // Wait for the socket to appear.
    for _ in 0..100 {
        if socket.exists() {
            return DaemonProc {
                socket,
                child,
                _data: state_dir.keep(),
                data_dir,
                records,
                log_path,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Give-up path: do not leak the daemon process.
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon socket never appeared");
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn expect_ok(
    response: Result<kallip_daemon_common::wire::Response, kallip_daemon_client::ClientError>,
) -> OkPayload {
    let response = response.expect("client exchange");
    match response.body {
        ResponseBody::Ok { payload } => payload,
        ResponseBody::Err { code, message } => {
            panic!("expected ok, got {code:?}: {message}")
        }
    }
}

#[test]
fn spawn_health_stop_round_trip() {
    let daemon = start_daemon();
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    // Spawn a real tagma: direct/local mode (no relay env) — the relay
    // plan resolution fails fast with neither configured... which is a
    // boot failure, proving the rollback path too if it ever regresses.
    // Minimal boot env: operator token plus the LLM profile trio the
    // runtime requires before it serves (PROVIDER/MODEL/API_KEY).
    let spawn = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "e2e".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![
            "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
            "KALLIP_LLM_PROVIDER=deepseek".into(),
            "KALLIP_LLM_MODEL=test-model".into(),
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
        ],
        exe: None,
        user: None,
    }));
    let OkPayload::Spawn { slug, pid, port } = expect_ok(spawn) else {
        panic!("expected spawn payload");
    };
    assert_eq!(slug, "e2e");
    assert!(port > 0, "real bound port");
    assert!(PathBuf::from(format!("/proc/{pid}")).exists(), "pid alive");

    // Health: running.
    let health = tokio_block_on(client.call(RequestBody::Health {
        slug: Some("e2e".into()),
    }));
    let OkPayload::Health { report } = expect_ok(health) else {
        panic!("expected health payload");
    };
    assert!(report.running, "spawned instance is running");
    assert_eq!(report.state, InstanceState::Running);

    // The record area carries the registration the scan enumerates.
    let instance_dir = daemon.data_dir.path().join("kallipai/tagmata/e2e");
    assert!(daemon.records.join("e2e.json").exists());
    for stray in ["instance.id", "owner", "pid", "port", "workspace"] {
        assert!(
            !instance_dir.join(stray).exists(),
            "stray state file {stray} must not appear"
        );
    }
    // The spawn recorded the requesting peer (this test process) as owner
    // in the record, next to the pointer at its instance dir.
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.records.join("e2e.json")).expect("record"),
    )
    .expect("parse record");
    assert_eq!(
        record["owner_uid"],
        serde_json::json!(unsafe { libc::getuid() })
    );
    assert_eq!(
        record["workspace"],
        serde_json::json!(workspace.path().display().to_string())
    );
    assert!(
        record["instance_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(
        record["data_dir"],
        serde_json::json!(instance_dir.display().to_string()),
        "the record points at the instance dir"
    );
    // The launch claim point pinned the kernel incarnation: the anchor
    // names this pid with a real start time and a wall-clock stamp.
    let identity = &record["identity"];
    assert_eq!(
        identity["pid"],
        serde_json::json!(pid),
        "anchor names the spawned pid"
    );
    assert!(
        identity["starttime"].as_u64().is_some_and(|t| t > 0),
        "anchor carries a kernel start time"
    );
    assert!(
        identity["anchored_at"].as_u64().is_some_and(|t| t > 0),
        "anchor carries its wall-clock stamp"
    );

    // Stop: TERM grace.
    let stop = tokio_block_on(client.call(RequestBody::Stop { slug: "e2e".into() }));
    let OkPayload::Stop { slug: stopped } = expect_ok(stop) else {
        panic!("expected stop payload");
    };
    assert_eq!(stopped, "e2e");
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "tagma exited after stop"
    );

    // Health after stop: not running, but the registration persists
    // (stop does not deregister; the record and the instance dir stay).
    let after = tokio_block_on(client.call(RequestBody::Health {
        slug: Some("e2e".into()),
    }));
    let OkPayload::Health { report } = expect_ok(after) else {
        panic!("expected health payload");
    };
    assert!(!report.running);
    assert_eq!(report.state, InstanceState::Dead);
    assert!(instance_dir.exists(), "instance dir survives stop");

    // Start: relaunch from the surviving tree. The fresh pid proves a new
    // process (not the old one lingering); the health gate re-opens.
    let started = tokio_block_on(client.call(RequestBody::Start {
        slug: "e2e".into(),
        env: vec!["KALLIP_TAGMA_LOG_TO_STDERR=1".into()],
        exe: None,
    }));
    let OkPayload::Spawn {
        slug: started_slug,
        pid: started_pid,
        port: started_port,
    } = expect_ok(started)
    else {
        panic!("expected start payload");
    };
    assert_eq!(started_slug, "e2e");
    assert_ne!(started_pid, pid, "a fresh incarnation, not the old one");
    assert!(started_port > 0, "fresh bound port");

    // The survived registration carried the spawn-time env over (relaunch
    // config), and credentials/ persists so the enrolled identity revives.
    let record_after: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.records.join("e2e.json")).expect("record after"),
    )
    .expect("parse record after start");
    assert_eq!(
        record_after["env"][0],
        serde_json::json!("KALLIP_OPERATOR_TOKEN=test-op-token")
    );
    assert_eq!(
        record_after["env"].as_array().expect("env array").len(),
        4,
        "one-shot overlay is not persisted to the record"
    );
    // The relaunch re-anchored: the previous incarnation's anchor was
    // overwritten with the fresh pid at the claim point.
    assert_eq!(
        record_after["identity"]["pid"],
        serde_json::json!(started_pid),
        "relaunch re-anchored to the fresh incarnation"
    );
    assert!(
        instance_dir.join("credentials").exists(),
        "credentials survive"
    );

    // Starting the now-running instance again is the conflict case.
    let code = match tokio_block_on(client.call(RequestBody::Start {
        slug: "e2e".into(),
        env: Vec::new(),
        exe: None,
    }))
    .expect("double start response")
    .body
    {
        ResponseBody::Err { code, .. } => code,
        other => panic!("expected slug_taken conflict, got {other:?}"),
    };
    assert_eq!(code, ErrorCode::SlugTaken);

    // Cleanup so the test does not leave a live tagma behind.
    let stopped_again = tokio_block_on(client.call(RequestBody::Stop { slug: "e2e".into() }));
    let OkPayload::Stop {
        slug: stopped_again_slug,
    } = expect_ok(stopped_again)
    else {
        panic!("expected stop payload");
    };
    assert_eq!(stopped_again_slug, "e2e");

    // Quiet-Dead: after the graceful exit, repeated polls classify
    // the dead pid as Gone with zero degraded-match warnings — a dead
    // pid must never trip the "matches only by name" warn.
    for _ in 0..2 {
        let poll = tokio_block_on(client.call(RequestBody::Health {
            slug: Some("e2e".into()),
        }));
        let OkPayload::Health { report } = expect_ok(poll) else {
            panic!("expected health payload");
        };
        assert_eq!(report.state, InstanceState::Dead);
    }
    let log = std::fs::read_to_string(&daemon.log_path).expect("daemon log");
    assert!(
        !log.contains("matches tagma only by name"),
        "degraded-match warn must stay silent on a healthy lifecycle:\n{log}"
    );
}

#[test]
fn start_filters_consumed_enrollment_code() {
    let daemon = start_daemon();
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    // Spawn local-only (the real enrolled setup is fabricated below — the
    // test has no archeion).
    let spawn = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "stale-code".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![
            "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
            "KALLIP_LLM_PROVIDER=deepseek".into(),
            "KALLIP_LLM_MODEL=test-model".into(),
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
        ],
        exe: None,
        user: None,
    }));
    let OkPayload::Spawn { pid, .. } = expect_ok(spawn) else {
        panic!("expected spawn payload");
    };

    // Stop and wait for the exit, so Start relaunches rather than
    // conflicting with a live instance.
    let stop = tokio_block_on(client.call(RequestBody::Stop {
        slug: "stale-code".into(),
    }));
    expect_ok(stop);
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Fabricate the bug's exact state: a spawn-time enrollment code still
    // persisted in the daemon's registration record plus credentials
    // stored by a completed enrollment. The archeion points at a port
    // nothing listens on — the real tagma boots through the Stored branch
    // and the entry merely degrades to local-only; replaying the code
    // instead makes tagma fail fast on stored-credentials-plus-code and
    // Start times out.
    let instance_dir = daemon.data_dir.path().join("kallipai/tagmata/stale-code");
    let record_path = daemon.records.join("stale-code.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).expect("record"))
            .expect("parse record");
    let env = record["env"].as_array_mut().expect("env array");
    env.push("KALLIP_TAGMA_RELAY_ARCHEION_URL=http://127.0.0.1:9".into());
    env.push("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-spent".into());
    std::fs::write(
        &record_path,
        serde_json::to_string(&record).expect("serialize record"),
    )
    .expect("rewrite record");
    let entry = instance_dir.join("credentials").join("default");
    std::fs::create_dir_all(&entry).expect("create credentials entry");
    std::fs::write(entry.join("tagma.id"), "tagma-1").expect("write tagma.id");
    std::fs::write(entry.join("tagma.token"), "token").expect("write tagma.token");

    let started = tokio_block_on(client.call(RequestBody::Start {
        slug: "stale-code".into(),
        env: Vec::new(),
        exe: None,
    }));
    let OkPayload::Spawn {
        pid: started_pid,
        port: started_port,
        ..
    } = expect_ok(started)
    else {
        panic!("expected start payload");
    };
    assert!(started_port > 0, "fresh bound port");
    assert_ne!(started_pid, pid, "a fresh incarnation");

    // The scrub removed the spent code from the persisted record while
    // the rest of the env survived (the archeion url is not secret
    // material and stays — the Stored branch needs it).
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).expect("record after"))
            .expect("parse record after");
    let env = after["env"].as_array().expect("env array after");
    assert!(env.contains(&serde_json::json!("KALLIP_OPERATOR_TOKEN=test-op-token")));
    assert!(env.contains(&serde_json::json!(
        "KALLIP_TAGMA_RELAY_ARCHEION_URL=http://127.0.0.1:9"
    )));
    assert!(!env.iter().any(|pair| {
        pair.as_str()
            .is_some_and(|p| p.starts_with("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE="))
    }));

    // Cleanup so the test does not leave a live tagma behind.
    let stop = tokio_block_on(client.call(RequestBody::Stop {
        slug: "stale-code".into(),
    }));
    expect_ok(stop);
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{started_pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !PathBuf::from(format!("/proc/{started_pid}")).exists(),
        "relaunched tagma exited after stop"
    );
}

#[test]
fn spawn_rejects_slug_reuse_and_workspace_overlap() {
    let daemon = start_daemon();
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace");

    let first = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "taken".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![
            "KALLIP_OPERATOR_TOKEN=t".into(),
            "KALLIP_LLM_PROVIDER=deepseek".into(),
            "KALLIP_LLM_MODEL=test-model".into(),
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
        ],
        exe: None,
        user: None,
    }));
    assert!(matches!(
        first.expect("first spawn").body,
        ResponseBody::Ok { .. }
    ));
    // Stop it so the second spawn's liveness wait cannot leak.
    let _ = tokio_block_on(client.call(RequestBody::Stop {
        slug: "taken".into(),
    }));

    let reuse = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "taken".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![],
        exe: None,
        user: None,
    }));
    match reuse.expect("reuse exchange").body {
        ResponseBody::Err { code, .. } => assert_eq!(code, ErrorCode::SlugTaken),
        other => panic!("expected slug_taken, got {other:?}"),
    }

    // Same workspace under a different slug = overlap.
    let overlap = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "other".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![],
        exe: None,
        user: None,
    }));
    match overlap.expect("overlap exchange").body {
        ResponseBody::Err { code, .. } => assert_eq!(code, ErrorCode::WorkspaceOverlap),
        other => panic!("expected workspace_overlap, got {other:?}"),
    }
}

#[test]
fn start_recovers_from_stale_runtime_json() {
    // The failure mode this locks: a leftover runtime.json from a previous
    // incarnation must not poison the launch poll, and its recorded pid
    // (required to look like a tagma before it is trusted) must not decide
    // between a bogus match and a 30s kill. The launch clears the
    // leftover before starting the helper, so the poll only ever sees
    // this launch's self-report.
    let daemon = start_daemon();
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let spawn = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "stale-runtime".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![
            "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
            "KALLIP_LLM_PROVIDER=deepseek".into(),
            "KALLIP_LLM_MODEL=test-model".into(),
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
        ],
        exe: None,
        user: None,
    }));
    let OkPayload::Spawn { pid, .. } = expect_ok(spawn) else {
        panic!("expected spawn payload");
    };

    // Stop and wait for the exit, so the fabricated runtime.json below
    // carries a genuinely dead pid — exactly what a crash leaves behind.
    let stop = tokio_block_on(client.call(RequestBody::Stop {
        slug: "stale-runtime".into(),
    }));
    expect_ok(stop);
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let stale = serde_json::json!({ "pid": pid, "port": 1, "starttime": 1 });
    let instance_dir = daemon
        .data_dir
        .path()
        .join("kallipai/tagmata/stale-runtime");
    std::fs::write(
        instance_dir.join("runtime.json"),
        serde_json::to_vec(&stale).expect("serialize stale runtime"),
    )
    .expect("write stale runtime.json");

    let started = tokio_block_on(client.call(RequestBody::Start {
        slug: "stale-runtime".into(),
        env: Vec::new(),
        exe: None,
    }));
    let OkPayload::Spawn {
        pid: started_pid,
        port: started_port,
        ..
    } = expect_ok(started)
    else {
        panic!("expected start payload");
    };
    assert_ne!(started_pid, pid, "the stale pid, not this launch");
    assert!(started_port > 0, "fresh bound port");

    // Cleanup so the test does not leave a live tagma behind.
    let stop = tokio_block_on(client.call(RequestBody::Stop {
        slug: "stale-runtime".into(),
    }));
    expect_ok(stop);
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{started_pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !PathBuf::from(format!("/proc/{started_pid}")).exists(),
        "relaunched tagma exited after stop"
    );
}

#[test]
fn start_rejects_when_stale_runtime_names_a_live_pid() {
    // Liveness alone decides AlreadyRunning: a leftover naming a live pid
    // (here: this test process) is refused, not taken over and not killed.
    // Under the old comm re-check this same shape wedged the poll into a
    // 30s timeout whose kill branch SIGKILLed this very pid.
    let daemon = start_daemon();
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let spawn = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "live-stale".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![
            "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
            "KALLIP_LLM_PROVIDER=deepseek".into(),
            "KALLIP_LLM_MODEL=test-model".into(),
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
        ],
        exe: None,
        user: None,
    }));
    let OkPayload::Spawn { pid, .. } = expect_ok(spawn) else {
        panic!("expected spawn payload");
    };

    let stop = tokio_block_on(client.call(RequestBody::Stop {
        slug: "live-stale".into(),
    }));
    expect_ok(stop);
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // A live pid that is not a tagma: the conservative refusal is the
    // point. Refusing costs one retry; matching or killing an unrelated
    // process costs the process.
    let stale = serde_json::json!({ "pid": std::process::id(), "port": 1, "starttime": 1 });
    let instance_dir = daemon.data_dir.path().join("kallipai/tagmata/live-stale");
    std::fs::write(
        instance_dir.join("runtime.json"),
        serde_json::to_vec(&stale).expect("serialize stale runtime"),
    )
    .expect("write stale runtime.json");

    let started = tokio_block_on(client.call(RequestBody::Start {
        slug: "live-stale".into(),
        env: Vec::new(),
        exe: None,
    }));
    let response = started.expect("client exchange");
    match response.body {
        ResponseBody::Err { code, .. } => assert_eq!(code, ErrorCode::SlugTaken),
        other => panic!("expected slug_taken refusal, got {other:?}"),
    }
}

/// A manually booted (daemon-less) tagma is legal — it just has to name
/// itself. With KALLIP_TAGMA_SLUG set it derives its data root under the fake
/// XDG data home and publishes runtime.json there unconditionally:
/// every boot owns an instance dir. A successful
/// TCP connect proves the boot got past identity resolution.
#[test]
fn manual_boot_with_slug_publishes_runtime_json() {
    let data_home = tempfile::tempdir().expect("data home tempdir");
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let port = probe.local_addr().expect("probe addr").port();
    drop(probe);
    let instance_dir = data_home
        .path()
        .join("kallipai")
        .join("tagmata")
        .join("manual");
    let mut tagma = std::process::Command::new(resolve_bin("kallip-tagma"))
        .env("KALLIP_TAGMA_SLUG", "manual")
        .env("XDG_DATA_HOME", data_home.path())
        .env("KALLIP_TAGMA_ADDR", format!("127.0.0.1:{port}"))
        .env("KALLIP_OPERATOR_TOKEN", "test-op-token")
        .env("KALLIP_LLM_PROVIDER", "deepseek")
        .env("KALLIP_LLM_MODEL", "test-model")
        .env("KALLIP_LLM_DEEPSEEK_API_KEY", "test-key")
        // Test-env isolation: a machine running inside the kallipai
        // stack (e.g. an agent) carries ambient KALLIP_TAGMA_RELAY_*
        // vars; leaked into the child they trip the tagma relay
        // fail-fast and this local-only boot never listens.
        .env_remove("KALLIP_TAGMA_RELAY_ARCHEION_URL")
        .env_remove("KALLIP_TAGMA_RELAY_LESCHE_URL")
        .env_remove("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("manual tagma boot");
    let mut connected = false;
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            connected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = tagma.kill();
    let _ = tagma.wait();
    assert!(connected, "manual tagma never listened on {port}");
    assert!(
        instance_dir.join("runtime.json").exists(),
        "a slug-named boot publishes runtime.json into its instance dir"
    );
}

/// The stop guard's two legs: a runtime.json retargeted at a foreign
/// live pid is refused even though the pid is alive (the anchor names
/// a different incarnation), and once the tree is restored the same
/// stop succeeds because the anchor verifies the original pid.
#[test]
fn stop_refuses_tampered_runtime_pid_then_allows_restored() {
    let daemon = start_daemon();
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let spawn = tokio_block_on(client.call(RequestBody::Spawn {
        slug: "tamper".into(),
        workspace: workspace.path().display().to_string(),
        env: vec![
            "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
            "KALLIP_LLM_PROVIDER=deepseek".into(),
            "KALLIP_LLM_MODEL=test-model".into(),
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
        ],
        exe: None,
        user: None,
    }));
    let OkPayload::Spawn { pid, port, .. } = expect_ok(spawn) else {
        panic!("expected spawn payload");
    };
    let instance_dir = daemon.data_dir.path().join("kallipai/tagmata/tamper");
    let runtime_path = instance_dir.join("runtime.json");
    let original = std::fs::read_to_string(&runtime_path).expect("read runtime.json");

    // Tamper: point runtime.json at this test process — alive, but
    // provably not the anchored incarnation.
    let tampered = serde_json::json!({ "pid": std::process::id(), "port": port, "starttime": 1 });
    std::fs::write(
        &runtime_path,
        serde_json::to_vec(&tampered).expect("serialize tampered runtime"),
    )
    .expect("write tampered runtime.json");
    let refused = tokio_block_on(client.call(RequestBody::Stop {
        slug: "tamper".into(),
    }));
    match refused.expect("refused exchange").body {
        ResponseBody::Err { code, .. } => assert_eq!(code, ErrorCode::NotRunning),
        other => panic!("expected not_running refusal, got {other:?}"),
    }
    // The refusal must not have killed the real tagma.
    assert!(
        PathBuf::from(format!("/proc/{pid}")).exists(),
        "real tagma alive after refused stop"
    );

    // Restore: the anchor verifies the original pid and stop lands.
    std::fs::write(&runtime_path, original).expect("restore runtime.json");
    let stopped = tokio_block_on(client.call(RequestBody::Stop {
        slug: "tamper".into(),
    }));
    expect_ok(stopped);
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "tagma exited after restored stop"
    );
}
/// Drive a tokio client call from a sync test: a minimal single-thread
/// runtime per call. (The daemon crate is async; the tests are not.)
fn tokio_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}
