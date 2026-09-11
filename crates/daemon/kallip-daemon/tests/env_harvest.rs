//! Login-environment harvest, observed end to end through a real daemon
//! and a real tagma: each launch harvests the executing user's login
//! environment, the instance env carries it, explicit request pairs win
//! per key, daemon-owned keys stay owned, restarts re-harvest, and nothing
//! harvest-shaped is persisted.
//!
//! The daemon's HOME is pointed at a fixture home whose .profile stands in
//! for "this user's login PATH carries the kallip installation" — the
//! marker directory plays the role the kallip bin directory plays in a
//! real deployment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kallip_daemon_client::DaemonClient;
use kallip_daemon_common::wire::{OkPayload, RequestBody, ResponseBody};

// Same resolution chain as the lifecycle tests: KALLIP_BIN_DIR →
// CARGO_BIN_EXE_* → target-dir parent → PATH. A package-scoped build
// does not produce the workspace binaries; build the workspace first.
fn resolve_bin(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("KALLIP_BIN_DIR")
        && let p = Path::new(&dir).join(name)
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
        .and_then(|e| e.parent().map(Path::to_path_buf))
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

struct DaemonProc {
    socket: PathBuf,
    child: std::process::Child,
    _data_dir: tempfile::TempDir,
    records: PathBuf,
    _state: tempfile::TempDir,
}

/// Boot a daemon whose harvest seed HOME is the fixture home: its login
/// shell therefore reads the fixture .profile.
fn start_daemon_with_home(home: &Path) -> DaemonProc {
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let socket = state_dir.path().join("control.sock");
    let records = state_dir.path().join("records");
    let bin = resolve_bin("kallip-daemon");
    let mut child = std::process::Command::new(&bin)
        .env("XDG_DATA_HOME", data_dir.path())
        .env(
            "KALLIP_BIN_DIR",
            resolve_bin("kallip-tagma")
                .parent()
                // &Path has no Default; the empty path falls through to bare-name
                // PATH lookup.
                .unwrap_or(Path::new("")),
        )
        // Pin the record area in the state tempdir: the default
        // derivation would follow the fixture HOME (isolated, but the
        // explicit override keeps where records land obvious).
        .env("KALLIP_DAEMON_RECORD_DIR", state_dir.path().join("records"))
        .env(
            "KALLIP_DAEMON_SOCKET",
            state_dir.path().join("control.sock"),
        )
        .env("HOME", home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");
    for _ in 0..100 {
        if socket.exists() {
            return DaemonProc {
                socket,
                child,
                _data_dir: data_dir,
                records,
                _state: state_dir,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// The fixture home: its .profile replaces PATH wholesale (the marker bin
/// directory stands in for the kallip installation location), exports a
/// full-harvest marker, and a harvested RUST_LOG — the probes assert
/// what the instance env carries.
fn fixture_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::write(
        home.path().join(".profile"),
        "export PATH=\"/kallip-harvest-fixture/bin:/usr/bin:/bin\"\n\
         export KALLIP_HARVEST_MARKER=stage-one\n\
         export RUST_LOG=harvest-level\n",
    )
    .expect("write fixture profile");
    home
}

fn harvest_bash_exists() -> bool {
    std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg("true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn boot_env(extra: &[&str]) -> Vec<String> {
    let mut env = vec![
        "KALLIP_OPERATOR_TOKEN=test-op-token".to_owned(),
        "KALLIP_LLM_PROVIDER=deepseek".to_owned(),
        "KALLIP_LLM_MODEL=test-model".to_owned(),
        "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".to_owned(),
    ];
    env.extend(extra.iter().map(|s| (*s).to_owned()));
    env
}

// The daemon's own passwd name, passed explicitly: an *inferred*
// in-place identity is refused for a root daemon, and these tests
// exercise harvest mechanics on any host. None (the old default)
// still works for a non-root daemon, whose inference is allowed.
fn daemon_user() -> Option<String> {
    let passwd = unsafe { libc::getpwuid(libc::geteuid()) };
    if passwd.is_null() {
        return None;
    }
    let name = unsafe { (*passwd).pw_name };
    if name.is_null() {
        return None;
    }
    Some(
        unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}
fn exchange(
    client: &DaemonClient,
    request: RequestBody,
) -> Result<kallip_daemon_common::wire::Response, kallip_daemon_client::ClientError> {
    tokio_block_on(client.call(request))
}

fn expect_ok(
    response: Result<kallip_daemon_common::wire::Response, kallip_daemon_client::ClientError>,
) -> OkPayload {
    match response.expect("client exchange").body {
        ResponseBody::Ok { payload } => payload,
        ResponseBody::Err { code, message } => {
            panic!("expected ok, got {code:?}: {message}")
        }
    }
}

fn spawn_instance(client: &DaemonClient, workspace: &Path, extra: &[&str]) -> u32 {
    let OkPayload::Spawn { pid, .. } = expect_ok(exchange(
        client,
        RequestBody::Spawn {
            slug: "harvest".to_owned(),
            workspace: workspace.display().to_string(),
            env: boot_env(extra),
            exe: None,
            user: daemon_user(),
        },
    )) else {
        panic!("expected spawn payload");
    };
    pid
}

fn stop_and_wait(client: &DaemonClient, pid: u32) {
    expect_ok(exchange(
        client,
        RequestBody::Stop {
            slug: "harvest".to_owned(),
        },
    ));
    for _ in 0..100 {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("tagma {pid} did not exit after stop");
}

fn read_environ(pid: u32) -> Vec<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).expect("read environ");
    raw.split(|&b| b == 0)
        .filter(|token| !token.is_empty())
        .map(|token| String::from_utf8_lossy(token).into_owned())
        .collect()
}

fn get<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
}

fn record_env(records: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(records.join("harvest.json")).expect("record");
    serde_json::from_str::<serde_json::Value>(&text).expect("parse record")["env"].clone()
}

#[test]
fn harvest_supplies_login_env_and_daemon_keys_stay_owned() {
    if !harvest_bash_exists() {
        eprintln!("skip: no /bin/bash on this host");
        return;
    }
    let home = fixture_home();
    let daemon = start_daemon_with_home(home.path());
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let pid = spawn_instance(&client, workspace.path(), &[]);
    let env = read_environ(pid);

    // The fixture kallip location rides the harvested PATH unmodified —
    // an agent shell resolves binaries exactly as a login shell would.
    assert_eq!(
        get(&env, "PATH"),
        Some("/kallip-harvest-fixture/bin:/usr/bin:/bin")
    );
    // Full harvest, not PATH-only: the profile marker arrives, and so does
    // HOME -- both ride the same harvest pass.
    assert_eq!(get(&env, "KALLIP_HARVEST_MARKER"), Some("stage-one"));
    let home_str = home.path().display().to_string();
    assert_eq!(get(&env, "HOME"), Some(home_str.as_str()));
    // The harvested RUST_LOG is used as-is; the daemon default must not
    // shadow it.
    assert_eq!(get(&env, "RUST_LOG"), Some("harvest-level"));
    assert_eq!(get(&env, "KALLIP_TAGMA_SLUG"), Some("harvest"));
    let workspace_canon = workspace.path().canonicalize().expect("canonicalize");
    let ws_str = workspace_canon.display().to_string();
    assert_eq!(get(&env, "KALLIP_WORKSPACE_ROOT"), Some(ws_str.as_str()));
    assert_eq!(get(&env, "KALLIP_TAGMA_ADDR"), Some("127.0.0.1:0"));

    stop_and_wait(&client, pid);
}

#[test]
fn explicit_pairs_win_over_harvested_values() {
    if !harvest_bash_exists() {
        eprintln!("skip: no /bin/bash on this host");
        return;
    }
    let home = fixture_home();
    let daemon = start_daemon_with_home(home.path());
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let pid = spawn_instance(
        &client,
        workspace.path(),
        &["PATH=/explicit-only-bin", "RUST_LOG=debug"],
    );
    let env = read_environ(pid);
    assert_eq!(get(&env, "PATH"), Some("/explicit-only-bin"));
    assert_eq!(get(&env, "RUST_LOG"), Some("debug"));
    assert_eq!(
        get(&env, "KALLIP_HARVEST_MARKER"),
        Some("stage-one"),
        "keys without an explicit pair keep the harvested value"
    );
    stop_and_wait(&client, pid);
}

#[test]
fn restart_reharvests_following_profile_changes() {
    if !harvest_bash_exists() {
        eprintln!("skip: no /bin/bash on this host");
        return;
    }
    let home = fixture_home();
    let daemon = start_daemon_with_home(home.path());
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let pid = spawn_instance(&client, workspace.path(), &[]);
    assert_eq!(
        get(&read_environ(pid), "KALLIP_HARVEST_MARKER"),
        Some("stage-one")
    );
    stop_and_wait(&client, pid);

    std::fs::write(
        home.path().join(".profile"),
        "export PATH=\"/kallip-harvest-fixture/bin:/usr/bin:/bin\"\n\
         export KALLIP_HARVEST_MARKER=stage-one\n\
         export RUST_LOG=harvest-level\n\
         export KALLIP_HARVEST_MARKER=stage-two\n",
    )
    .expect("rewrite fixture profile");

    let OkPayload::Spawn {
        pid: relaunched, ..
    } = expect_ok(exchange(
        &client,
        RequestBody::Start {
            slug: "harvest".to_owned(),
            env: Vec::new(),
            exe: None,
        },
    ))
    else {
        panic!("expected start payload");
    };
    assert_ne!(relaunched, pid, "a fresh incarnation");
    assert_eq!(
        get(&read_environ(relaunched), "KALLIP_HARVEST_MARKER"),
        Some("stage-two"),
        "the relaunch harvested the changed profile"
    );
    stop_and_wait(&client, relaunched);
}

#[test]
fn restart_overlay_reaches_the_relaunched_env() {
    if !harvest_bash_exists() {
        eprintln!("skip: no /bin/bash on this host");
        return;
    }
    let home = fixture_home();
    let daemon = start_daemon_with_home(home.path());
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let pid = spawn_instance(&client, workspace.path(), &["PATH=/persisted-bin"]);
    stop_and_wait(&client, pid);

    let OkPayload::Spawn {
        pid: relaunched, ..
    } = expect_ok(exchange(
        &client,
        RequestBody::Start {
            slug: "harvest".to_owned(),
            env: vec!["PATH=/overlay-bin".to_owned()],
            exe: None,
        },
    ))
    else {
        panic!("expected start payload");
    };
    assert_eq!(
        get(&read_environ(relaunched), "PATH"),
        Some("/overlay-bin"),
        "the one-shot overlay wins over the persisted pair"
    );
    stop_and_wait(&client, relaunched);
}

#[test]
fn harvested_env_is_not_persisted() {
    if !harvest_bash_exists() {
        eprintln!("skip: no /bin/bash on this host");
        return;
    }
    let home = fixture_home();
    let daemon = start_daemon_with_home(home.path());
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let pid = spawn_instance(&client, workspace.path(), &["PATH=/explicit-only-bin"]);
    let expected: serde_json::Value = boot_env(&["PATH=/explicit-only-bin"]).into();
    assert_eq!(
        record_env(&daemon.records),
        expected,
        "the record carries only the explicit request pairs"
    );
    stop_and_wait(&client, pid);

    let OkPayload::Spawn {
        pid: relaunched, ..
    } = expect_ok(exchange(
        &client,
        RequestBody::Start {
            slug: "harvest".to_owned(),
            env: Vec::new(),
            exe: None,
        },
    ))
    else {
        panic!("expected start payload");
    };
    assert_eq!(
        record_env(&daemon.records),
        expected,
        "a restart still persists nothing harvest-shaped"
    );
    stop_and_wait(&client, relaunched);
}

/// The wedge shape, end to end: a profile background job that
/// holds the harvest's stdout pipe must not hold the spawn RPC — the
/// daemon kills the whole process group, degrades to the fallback PATH,
/// and the instance still boots. (Unfixed, the RPC blocks for the
/// job's full lifetime: the probe measured 20.1s and this bound goes
/// red.)
#[test]
fn spawn_survives_a_background_pipe_holder() {
    if !harvest_bash_exists() {
        eprintln!("skip: no /bin/bash on this host");
        return;
    }
    let home = fixture_home();
    let profile = home.path().join(".profile");
    let mut text = std::fs::read_to_string(&profile).expect("read fixture profile");
    text.push_str("( read -t 20 x < /dev/zero ) &\n");
    std::fs::write(&profile, text).expect("append wedge line");
    let daemon = start_daemon_with_home(home.path());
    let client = DaemonClient::new(&daemon.socket);
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let started = std::time::Instant::now();
    let pid = spawn_instance(&client, workspace.path(), &[]);
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "spawn RPC stayed bounded, took {:?}",
        started.elapsed()
    );
    // The instance booted on the fallback PATH (the wedge profile
    // poisoned the harvest) — degraded, not wedged.
    let env = read_environ(pid);
    assert!(
        get(&env, "PATH").is_some_and(|p| !p.is_empty()),
        "a usable PATH either way"
    );
    stop_and_wait(&client, pid);
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
