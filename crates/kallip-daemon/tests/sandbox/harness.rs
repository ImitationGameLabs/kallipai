//! Shared machinery for the sandbox integration scenarios: on-disk world
//! setup, the scripted wiremock LLM, daemon + `kallip-run` subprocess
//! control, and history/assertion helpers. The scenario bodies
//! ([`super::guest`], [`super::normal`], [`super::dirlock`]) compose these.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fixed operator token so we never need to parse the daemon banner.
const OPERATOR_TOKEN: &str = "test-operator-secret";
/// A secret value planted in `~/.ssh/id_testkey`; the Guest hide-hole must mask it.
pub const SECRET_KEY: &str = "SECRETKEY\n";
/// How long to wait for the daemon to accept connections before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// Skip guard
// ===========================================================================

/// True when the host cannot run the sandbox scenarios (non-Linux, no landlock,
/// or unprivileged user namespaces disabled). The tests call this and return
/// early on `true` so CI without a landlock kernel skips instead of failing.
pub fn unsupported() -> bool {
    // landlock compile-time gate is already enforced by the crate-level cfg in
    // main.rs; this is the runtime kernel probe reused from kallip-shell's
    // own test suite.
    if kallip_shell::landlock::ensure_supported().is_err() {
        eprintln!("sandbox: skipping, landlock unsupported on this kernel");
        return true;
    }
    if userns_disabled() {
        eprintln!("sandbox: skipping, unprivileged user namespaces disabled");
        return true;
    }
    false
}

/// Mirror of `kallip-shell::landlock::tests::userns_unavailable`: read the
/// sysctl; `0` means disabled. Absent => assume permitted.
fn userns_disabled() -> bool {
    matches!(
        std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone"),
        Ok(s) if s.trim() == "0"
    )
}

/// The scratch root used for the sandbox scenarios' `home`/`data`/`workspace`
/// dirs. It MUST live outside libsandbox's baseline-writable set (`/tmp`,
/// `/var/tmp`, `$TMPDIR`): the permission model treats those as writable for
/// *every* agent, so a tempdir under `/tmp` would mask the real write-denial
/// behavior under test. Placing the dirs under a tree the agent's landlock
/// domain has no baseline grant for keeps the assertions honest.
///
/// Defaults to the gitignored `<workspace>/.testdata` (resolved at compile
/// time from the daemon crate's manifest dir). The container overrides this
/// with `KALLIP_TESTDATA_DIR` pointing at a tmpfs mounted at `/testdata`
/// (also outside the baseline-writable set, so assertions stay honest there
/// too).
fn testdata_root() -> PathBuf {
    let dir = match std::env::var("KALLIP_TESTDATA_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.testdata"),
    };
    std::fs::create_dir_all(&dir).expect("create testdata scratch root");
    dir
}

// ===========================================================================
// On-disk world setup
// ===========================================================================

/// Every temp path a scenario needs, owning the `TempDir` guards.
pub struct World {
    home: TempDir,
    data: TempDir,
    /// The agent workspace root; scenarios interpolate this into commands.
    pub workspace: TempDir,
    /// `$XDG_CONFIG_HOME`; lives under `home/.config`.
    config_dir: PathBuf,
}

impl World {
    pub fn home_path(&self) -> &Path {
        self.home.path()
    }
    /// `$KALLIP_DATA_DIR` verbatim (data_dir_root uses the env var as-is).
    pub fn data_root(&self) -> PathBuf {
        self.data.path().to_path_buf()
    }
    fn profiles_file(&self) -> PathBuf {
        self.config_dir.join("kallip").join("profiles.toml")
    }

    /// Pre-create the fixtures the scenarios reference: a real `~/.ssh` key
    /// (so the Guest hide-hole has something to mask), the dir scenario 2 locks,
    /// and the `.gnupg`/`.aws` dirs (so Guest hide-holes resolve -- they require
    /// existing directories).
    pub fn setup() -> Self {
        // All dirs go under the [`testdata_root`] scratch root (outside the
        // baseline-writable set) so write-denial assertions are honest.
        // `TempDir::new_in` auto-cleans each dir on drop; the root itself is
        // either the gitignored `<workspace>/.testdata` (persists, empty between
        // runs) or the container's ephemeral `/testdata` tmpfs.
        let parent = testdata_root();
        let home = TempDir::new_in(&parent).expect("home tmpdir");
        let data = TempDir::new_in(&parent).expect("data tmpdir");
        let workspace = TempDir::new_in(&parent).expect("workspace tmpdir");
        let config_dir = home.path().join(".config");

        let ssh_dir = home.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::write(ssh_dir.join("id_testkey"), SECRET_KEY).unwrap();
        for sub in [".gnupg", ".aws", "writable_subdir"] {
            std::fs::create_dir_all(home.path().join(sub)).unwrap();
        }
        std::fs::create_dir_all(config_dir.join("kallip")).unwrap();

        Self {
            home,
            data,
            workspace,
            config_dir,
        }
    }
}

/// Write a two-tier `profiles.toml`: tier 0 (root) -> `parent_url`, tier 1
/// (subagents) -> `child_url`. Routing a subagent's LLM traffic to a separate
/// mock server is what makes the dirlock scenario deterministic (parent and
/// child never contend for the same scripted replies).
fn write_profiles(world: &World, parent_url: &str, child_url: &str) {
    let toml = format!(
        r#"
[endpoints.parent]
family = "openai-compatible"
api_key = "test-key"
base_url = "{parent_url}"

[endpoints.child]
family = "openai-compatible"
api_key = "test-key"
base_url = "{child_url}"

[[tiers]]
  [[tiers.profiles]]
  id = "parent/test-model"
  endpoint = "parent"
  model = "test-model"
  max_context_window = 128000

[[tiers]]
  [[tiers.profiles]]
  id = "child/test-model"
  endpoint = "child"
  model = "test-model"
  max_context_window = 128000
"#
    );
    let file = world.profiles_file();
    std::fs::write(&file, toml).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
}

// ===========================================================================
// Wiremock: SSE scripting
// ===========================================================================

/// A scripted LLM reply: either one `bash_exec` tool call, or a final assistant
/// content message that ends the run. Owns the command string so scenarios can
/// interpolate absolute paths (the agent's `$PWD` is the daemon process's cwd,
/// not the workspace, so commands must not rely on it).
pub enum Reply {
    Tool(String),
    End(&'static str),
}

/// Render an SSE body carrying a single complete `bash_exec` tool call (the
/// runtime accumulator accepts id+name+arguments in one delta), then the closing
/// `tool_calls` chunk, then `[DONE]`. Verified against `just-llm-client`'s
/// `DeltaMessage`/`ChatCompletionChunkToolCall` and the runtime's
/// `stream_accumulator`.
fn render_tool_call(call_id: &str, command: &str) -> String {
    // `arguments` is the stringified JSON of the bash_exec args; embedding it as
    // a JSON string here yields the correct double-escaping once the whole chunk
    // is serialized.
    let arguments = serde_json::to_string(&json!({ "command": command })).unwrap();
    let open = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": "bash_exec", "arguments": arguments }
                }]
            },
            "finish_reason": null
        }]
    });
    let close = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
    });
    format!("data: {open}\n\ndata: {close}\n\ndata: [DONE]\n\n")
}

/// Render an SSE body carrying a final assistant content message (no tool calls),
/// which makes the round loop exit `Finished` with `exit == "success"`.
fn render_end(content: &str) -> String {
    let open = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": { "content": content },
            "finish_reason": null
        }]
    });
    let close = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });
    format!("data: {open}\n\ndata: {close}\n\ndata: [DONE]\n\n")
}

fn sse_response(body: String) -> ResponseTemplate {
    // `set_body_raw` sets the body AND the content-type together (set_body_string
    // forces text/plain; the OpenAI-compat backend rejects non-event-stream
    // responses, which manifested as a transport error).
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// Mount a sequence of scripted replies on `server`, **forward order** (wiremock
/// matches first-mounted first; each reply `up_to_n_times(1)` so it is consumed
/// by exactly one POST). A trailing 500 catch-all makes any *unplanned* call fail
/// loudly (within-tier failover exhausts -> non-success exit) rather than silently
/// pass.
async fn mount_script(server: &MockServer, replies: &[Reply]) {
    for reply in replies {
        let body = match reply {
            Reply::Tool(cmd) => render_tool_call("call", cmd),
            Reply::End(msg) => render_end(msg),
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
    // Catch-all: an extra/unplanned POST must not silently succeed.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(server)
        .await;
}

/// Mount a permanent final-content reply on `server` (the subagent endpoint).
async fn mount_final(server: &MockServer, content: &str) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(render_end(content)))
        .mount(server)
        .await;
}

// ===========================================================================
// Daemon + runner subprocess control
// ===========================================================================

/// Owned daemon subprocess plus the captured stdout/stderr buffers (for
/// diagnostics on failure). `kill_on_drop` ensures the child dies if a guard
/// ever escapes explicit shutdown.
pub struct DaemonProc {
    child: Child,
    port: u16,
    url: String,
    stdout: Arc<std::sync::Mutex<Vec<u8>>>,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl DaemonProc {
    pub async fn kill(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    pub fn diagnostics(&self) -> String {
        let stdout = String::from_utf8_lossy(&self.stdout.lock().unwrap()).into_owned();
        let stderr = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
        format!("--- daemon stdout ---\n{stdout}\n--- daemon stderr ---\n{stderr}")
    }
}

/// Bind an ephemeral port, read it, drop the listener, and hand it to the daemon
/// (the daemon never echoes its bound port, so `127.0.0.1:0` is unusable for
/// discovery). The bind->drop->rebind window is tiny; on the rare collision the
/// daemon fails to bind and the readiness check surfaces its captured output.
fn alloc_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Resolve a workspace binary by name. The lookup order is:
///   1. `KALLIP_BIN_DIR` -- the container states the buildEnv `bin/`
///      explicitly (we cannot derive it from `current_exe`: `/proc/self/exe`
///      resolves the buildEnv symlink into a sub-store path that holds only
///      `sandbox`, not its siblings).
///   2. `CARGO_BIN_EXE_<name>` -- cargo injects this for same-package binaries
///      under `cargo test`.
///   3. One level out of `deps/` -- under `cargo test` the test binary lives at
///      `target/<profile>/deps/`, while the workspace binaries live at
///      `target/<profile>/`.
///   4. Bare name -- let the spawned process resolve it via PATH.
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

/// The dir holding the workspace binaries -- prepended to the daemon's PATH so
/// the agent's bash can invoke the `kallip` CLI (used by the normal/dirlock
/// scenarios via `dirlock`/`subagent`). Derived from the daemon binary's location.
fn bin_dir() -> PathBuf {
    resolve_bin("kallip-daemon")
        .parent()
        .expect("binary has a parent dir")
        .to_path_buf()
}

async fn drain(mut read: impl tokio::io::AsyncRead + Unpin, buf: Arc<std::sync::Mutex<Vec<u8>>>) {
    let mut tmp = [0u8; 1024];
    loop {
        match read.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.lock().unwrap().extend_from_slice(&tmp[..n]),
        }
    }
}

/// Spawn the daemon against the mocked LLM endpoints and wait for it to accept
/// connections. Panics with the daemon's captured output if it fails to come up.
async fn spawn_daemon(world: &World, permission_class: Option<&str>) -> DaemonProc {
    let port = alloc_port();
    let url = format!("http://127.0.0.1:{port}");

    // profiles.toml (pointing at the wiremock endpoints) is written by the
    // caller before this; the daemon resolves it via KALLIP_PROFILES_FILE.
    let mut env: Vec<(&str, String)> = vec![
        ("KALLIP_OPERATOR_TOKEN", OPERATOR_TOKEN.into()),
        ("KALLIP_DATA_DIR", world.data.path().display().to_string()),
        // The daemon eagerly creates the singleton root at startup from these
        // env vars (it owns the root; kallip-run only posts to it). The root's
        // workspace is the scenario workspace, and a generous round cap is a
        // safety net — scenarios terminate via the scripted terminal reply, not
        // the cap.
        (
            "KALLIP_WORKSPACE_ROOT",
            world.workspace.path().display().to_string(),
        ),
        ("KALLIP_MAX_TOOL_ROUNDS", "50".into()),
        ("HOME", world.home_path().display().to_string()),
        ("XDG_CONFIG_HOME", world.config_dir.display().to_string()),
        (
            "KALLIP_PROFILES_FILE",
            world.profiles_file().display().to_string(),
        ),
        ("KALLIP_POLICY_PRESET", "allow-all".into()),
        ("KALLIP_DAEMON_ADDR", format!("127.0.0.1:{port}")),
        ("KALLIP_ADVERTISE_URL", url.clone()),
        (
            "PATH",
            format!(
                "{}:{}",
                bin_dir().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        ),
        (
            "RUST_LOG",
            // Default to `info` (the daemon's own default) so startup errors
            // surface in the captured diagnostics on a readiness failure.
            option_env!("SANDBOX_LOG").unwrap_or("info").into(),
        ),
        ("NO_COLOR", "1".into()),
    ];
    if let Some(class) = permission_class {
        env.push(("KALLIP_ROOT_AGENT_PERMISSION_CLASS", class.into()));
    }

    let mut cmd = Command::new(resolve_bin("kallip-daemon"));
    cmd.args([
        "--listen-addr",
        &format!("127.0.0.1:{port}"),
        "--advertise-url",
        &url,
    ])
    .env_clear()
    .envs(env)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn kallip-daemon");
    let stdout = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stderr = Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(drain(child.stdout.take().unwrap(), stdout.clone()));
    tokio::spawn(drain(child.stderr.take().unwrap(), stderr.clone()));

    let proc = DaemonProc {
        child,
        port,
        url,
        stdout,
        stderr,
    };

    if !wait_ready(proc.port, READY_TIMEOUT).await {
        panic!("daemon did not become ready:\n{}", proc.diagnostics());
    }
    proc
}

/// Poll a TCP connect until the daemon is accepting connections, then settle
/// briefly: the listener is bound before `axum::serve` is ready to dispatch, so
/// a successful connect alone does not prove the router is live. The short
/// settle absorbs the accept-loop spin-up.
async fn wait_ready(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Parsed `kallip-run --json` output (camelCase, per `main.rs::JsonObject`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub agent_id: String,
    #[allow(dead_code)]
    assistant: String,
    pub exit: String,
}

/// Spawn `kallip-run` (a pure HTTP client -- it carries no sandbox logic) to
/// post the scenario prompt to the daemon's root agent. Captures its single JSON
/// stdout line.
pub async fn run_agent(daemon: &DaemonProc) -> RunResult {
    let output = Command::new(resolve_bin("kallip-run"))
        .args(["--prompt", "run the scripted sandbox checks", "--json"])
        .env_clear()
        .envs([
            ("KALLIP_DAEMON_URL", daemon.url.as_str()),
            ("KALLIP_AUTH_TOKEN", OPERATOR_TOKEN),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    bin_dir().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ])
        .output()
        .await
        .expect("spawn kallip-run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if std::env::var("SANDBOX_VERBOSE").is_ok() {
        eprintln!("=== kallip-run stdout ===\n{stdout}\n=== kallip-run stderr ===\n{stderr}");
    }
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let result: RunResult = serde_json::from_str(line).unwrap_or_else(|_| {
        panic!(
            "kallip-run produced no JSON; exit={}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code().unwrap_or(-1)
        )
    });
    result
}

// ===========================================================================
// History reading + assertions
// ===========================================================================

/// The subset of `BashExecOutput` we assert on (serde ignores the extra
/// `timed_out`/`truncated`/`cwd`/`task_id` fields). `output`/`stdout`/`stderr`
/// are all optional because `bash_exec`'s return shape depends on the `capture`
/// arg the LLM chose (`output` under `"merged"`, the default; `stdout`/`stderr`
/// under `"separate"`/`"stdout"`/`"stderr"`); assert via [`BashOut::text`] to stay
/// robust to whichever mode the model picked.
#[derive(Deserialize, Debug)]
pub struct BashOut {
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub stdout: Option<String>,
    #[serde(default)]
    pub stderr: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
}

impl BashOut {
    /// The captured command text regardless of which `capture` mode produced
    /// this result: the merged stream, else stdout, else stderr (whichever is
    /// present). Empty string when nothing was captured.
    pub fn text(&self) -> &str {
        self.output
            .as_deref()
            .or(self.stdout.as_deref())
            .or(self.stderr.as_deref())
            .unwrap_or("")
    }
}

/// Read all history records for an agent as raw JSON values.
pub fn history_records(data_root: &Path, agent_id: &str) -> Vec<Value> {
    let dir = data_root.join("agents").join(agent_id).join("history");
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.extension().is_some_and(|e| e == "ndjson") {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    out.push(v);
                }
            }
        }
    }
    out
}

/// `<data_root>/agents/<id>/meta.json` -- used for the data-tree write-denial check.
pub fn agent_meta_path(data_root: &Path, id: &str) -> PathBuf {
    data_root.join("agents").join(id).join("meta.json")
}

/// Extract `bash_exec` tool results in execution order. The runtime wraps each
/// tool result as `{"ok":..,"tool_name":"bash_exec","result":<BashExecOutput>}`;
/// we walk `role:"tool"` messages, match `tool_name`, and deserialize `.result`.
pub fn bash_results(records: &[Value]) -> Vec<BashOut> {
    let mut v = Vec::new();
    for rec in records {
        let Some(msgs) = rec.get("messages").and_then(|m| m.as_array()) else {
            continue;
        };
        for m in msgs {
            if m.get("role").and_then(|r| r.as_str()) != Some("tool") {
                continue;
            }
            let Some(content) = m.get("content").and_then(|c| c.as_str()) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<Value>(content) else {
                continue;
            };
            if val.get("tool_name").and_then(|t| t.as_str()) != Some("bash_exec") {
                continue;
            }
            if let Some(result) = val.get("result")
                && let Ok(b) = serde_json::from_value::<BashOut>(result.clone())
            {
                v.push(b);
            }
        }
    }
    v
}

/// Assert the Nth bash result matches an expectation. `success=true` expects
/// exit code 0; `false` expects any non-zero (EPERM/EROFS/etc.).
#[track_caller]
pub fn expect(results: &[BashOut], idx: usize, label: &str, success: bool) {
    let Some(out) = results.get(idx) else {
        panic!(
            "scenario step `{label}` (#{idx}) missing from history; got {} results",
            results.len()
        );
    };
    let code = out.exit_code.unwrap_or(-1);
    let ok = if success { code == 0 } else { code != 0 };
    if !ok {
        panic!(
            "scenario step `{label}` (#{idx}): expected {}, got exit_code={:?}\n--- output ---\n{}",
            if success { "success" } else { "failure" },
            out.exit_code,
            out.text(),
        );
    }
}

// ===========================================================================
// Shared scenario scaffold
// ===========================================================================

/// Everything a scenario body needs. Built by [`start`] from a caller-owned
/// `World` (the caller builds the script first so it can interpolate the
/// workspace path). Field order is load-bearing for panic safety: fields drop
/// in declaration order, so `daemon` (with `kill_on_drop`) drops *before* the
/// `MockServer`s and `world`'s `TempDir`s. The two `MockServer`s are held here
/// -- not dropped at the end of `start` -- so they outlive the daemon's in-flight
/// LLM calls (a pooled `MockServer` is only reclaimed on drop).
pub struct Fixture {
    pub daemon: DaemonProc,
    // Underscore-prefixed: held only to keep the pooled MockServer alive for
    // the daemon's in-flight LLM calls (their Drop reclaims it), never read.
    _parent_mock: MockServer,
    _child_mock: MockServer,
    pub world: World,
    pub data_root: PathBuf,
}

pub async fn start(
    world: World,
    parent_script: &[Reply],
    permission_class: Option<&str>,
) -> Fixture {
    let parent_mock = MockServer::start().await;
    let child_mock = MockServer::start().await;
    write_profiles(&world, &parent_mock.uri(), &child_mock.uri());
    mount_script(&parent_mock, parent_script).await;
    mount_final(&child_mock, "child-done").await;
    let data_root = world.data_root();
    let daemon = spawn_daemon(&world, permission_class).await;
    Fixture {
        daemon,
        _parent_mock: parent_mock,
        _child_mock: child_mock,
        world,
        data_root,
    }
}
