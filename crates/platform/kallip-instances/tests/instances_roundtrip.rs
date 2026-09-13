//! Full-stack round trip against a REAL daemon subprocess: the router
//! (token + host guards + proxy + mapping) driven end to end over oneshot,
//! with the daemon answering on a tempdir UDS socket.
//!
//! The spawn leg launches a real tagma binary: the daemon resolves its
//! helpers by bare name (KALLIP_BIN_DIR → PATH), so these tests hand
//! the daemon a KALLIP_BIN_DIR pinning the workspace build dir that
//! resolve_bin finds. Build the workspace (or at least `cargo build
//! -p kallip-daemon kallip-instances kallip`) before running: a
//! missing build is an environment fault, while a stale one is
//! reported by the contract tests as a version canary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use kallip_daemon_client::DaemonClient;
use kallip_instances::{AppState, build_router};
use tower::ServiceExt;

struct DaemonProc {
    socket: PathBuf,
    _data_dir: tempfile::TempDir,
    records: PathBuf,
    child: std::process::Child,
    _state_dir: tempfile::TempDir,
}

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

fn start_daemon() -> DaemonProc {
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let socket = state_dir.path().join("control.sock");
    let bin = resolve_bin("kallip-daemon");
    let mut child = std::process::Command::new(&bin)
        // Pin the record area in the state tempdir (the default
        // derivation would follow the host HOME); the XDG data anchor
        // isolates the instance trees.
        .env("KALLIP_DAEMON_RECORD_DIR", state_dir.path().join("records"))
        // The daemon owns the platform-origin default: the unit env carries
        // it, so the relay-intent e2e exercises the daemon-side fill
        // against it.
        .env("KALLIP_POLIS_URL", "http://localhost:8080")
        .env("XDG_DATA_HOME", data_dir.path())
        .env(
            "KALLIP_BIN_DIR",
            resolve_bin("kallip-tagma")
                .parent()
                // &Path has no Default; the empty path falls through to bare-name
                // PATH lookup.
                .unwrap_or(Path::new("")),
        )
        .env(
            "KALLIP_DAEMON_SOCKET",
            state_dir.path().join("control.sock"),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");
    for _ in 0..100 {
        if socket.exists() {
            // The daemon owns the data dir; the tempdirs must outlive the
            // reaped child. The guards live in DaemonProc, so both tempdirs
            // die with it.
            let records = state_dir.path().join("records");
            return DaemonProc {
                socket,
                _data_dir: data_dir,
                records,
                child,
                _state_dir: state_dir,
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

// The daemon's own passwd name, passed explicitly: an *inferred*
// in-place identity is refused for a root daemon (the guard asks
// for an explicit one), while None keeps working on a non-root
// host, whose inference is allowed.
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
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    // A Host is mandatory here: the router's host guard rejects requests
    // without one, exactly as a real browser would send it.
    builder = builder.header("host", "127.0.0.1:7300");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.unwrap_or("").to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.expect("send");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("read body");
    (
        status,
        String::from_utf8(bytes.to_bytes().to_vec()).expect("utf8"),
    )
}

#[tokio::test]
async fn full_management_round_trip_with_guards() {
    let daemon = start_daemon();
    // Drain the daemon's startup backlog with one direct exchange so the
    // first proxied call is not racing the listener.
    let probe = DaemonClient::new(&daemon.socket);
    let _ = probe
        .call(kallip_daemon_common::wire::RequestBody::List)
        .await;

    let state = AppState {
        backend: kallip_instances::backend::UdsBackend::arc(DaemonClient::new(&daemon.socket)),
        auth: kallip_instances::guard::AuthMode::Token("itest-token".into()),
        cors_origins: String::new(),
        allowed_hosts: vec![],
    };
    let app = build_router(state);

    // No token: 401.
    let (status, body) = send(&app, "GET", "/list", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("\"unauthorized\""), "{body}");
    // Capabilities sits behind the same guard: no token, no list.
    let (status, _body) = send(&app, "GET", "/capabilities", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Foreign Host: 403, even with a valid token.
    let request = Request::get("/list")
        .header("host", "evil.example")
        .header("authorization", "Bearer itest-token")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.expect("send");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Health (daemon itself): 200.
    let (status, body) = send(&app, "GET", "/health", Some("itest-token"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"running\":true"), "{body}");
    assert!(body.contains("\"state\":\"running\""), "{body}");

    // Spawn a real instance (minimal boot env from the daemon lifecycle
    // tests: operator token + the LLM profile trio).
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let spawn_body = serde_json::json!({
        "slug": "web-e2e",
        "workspace": workspace.path().display().to_string(),
        // An explicit launch identity: the sandbox daemon and this
        // test process share a uid, so the implicit self-launch is
        // refused and the explicit name is required.
        "user": daemon_user(),
        "env": [
            "KALLIP_OPERATOR_TOKEN=test-op-token",
            "KALLIP_LLM_PROVIDER=deepseek",
            "KALLIP_LLM_MODEL=test-model",
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key",
        ],
    })
    .to_string();
    let (status, body) = send(
        &app,
        "POST",
        "/spawn",
        Some("itest-token"),
        Some(&spawn_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"slug\":\"web-e2e\""), "{body}");
    assert!(body.contains("\"pid\":"), "{body}");
    assert!(body.contains("\"port\":"), "{body}");
    // An advertised-method fetch and an unsupported spawn method both
    // speak the capability vocabulary.
    let (status, body) = send(&app, "GET", "/capabilities", Some("itest-token"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("designated-user"), "{body}");
    let bad_method = serde_json::json!({
        "slug": "web-e2e",
        "workspace": "/tmp/itest",
        "method": "container",
    })
    .to_string();
    let (status, body) = send(
        &app,
        "POST",
        "/spawn",
        Some("itest-token"),
        Some(&bad_method),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("unsupported_method"), "{body}");

    // List sees it.
    let (status, body) = send(&app, "GET", "/list", Some("itest-token"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"web-e2e\""), "{body}");

    // Health for the slug: running.
    let (status, body) = send(
        &app,
        "GET",
        "/health?slug=web-e2e",
        Some("itest-token"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"running\":true"), "{body}");
    assert!(body.contains("\"state\":\"running\""), "{body}");

    // Unknown slug: the daemon's not_found maps to 404 with the code key.
    let (status, body) = send(
        &app,
        "GET",
        "/health?slug=missing",
        Some("itest-token"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("\"not_found\""), "{body}");

    // Stop it.
    let (status, body) = send(
        &app,
        "POST",
        "/stop",
        Some("itest-token"),
        Some(r#"{"slug":"web-e2e"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"web-e2e\""), "{body}");

    // Stop again: not_running maps to 409.
    let (status, body) = send(
        &app,
        "POST",
        "/stop",
        Some("itest-token"),
        Some(r#"{"slug":"web-e2e"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("\"not_running\""), "{body}");
}

// The relay-injection wiring through the real daemon: the same ruled
// states as the backend unit tests, but end to end -- the fill call
// on the spawn path cannot silently regress, the daemon's env
// validation (which rejects empty values) sees exactly what ships,
// and the persisted record carries the filled URLs into the
// start path.
#[tokio::test]
async fn relay_intent_spawn_persists_filled_urls() {
    let daemon = start_daemon();
    // Drain the startup backlog so the spawn exchange is not racing it.
    let _ = DaemonClient::new(&daemon.socket)
        .call(kallip_daemon_common::wire::RequestBody::List)
        .await;

    let backend = kallip_instances::backend::UdsBackend::arc(DaemonClient::new(&daemon.socket));
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let spawned = backend
        .spawn(
            "relay-e2e".into(),
            workspace.path().display().to_string(),
            vec![
                // Minimal boot env (as the lifecycle tests) plus a
                // bare enrollment code: relay intent with no URLs.
                // Activating the relay against the dead defaults
                // degrades to local-only; the spawn must still
                // succeed offline.
                "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
                "KALLIP_LLM_PROVIDER=deepseek".into(),
                "KALLIP_LLM_MODEL=test-model".into(),
                "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
                "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-x".into(),
            ],
            daemon_user(),
        )
        .await
        .expect("relay-intent spawn");
    assert_eq!(spawned.slug, "relay-e2e");

    let record_path = daemon.records.join("relay-e2e.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).expect("record"))
            .expect("parse record");
    let env = record["env"].as_array().expect("env array");
    assert_eq!(
        env.iter()
            .filter(|e| e
                .as_str()
                .is_some_and(|s| s.starts_with("KALLIP_POLIS_URL")))
            .count(),
        1,
        "exactly one KALLIP_POLIS_URL entry"
    );

    let _ = backend.stop("relay-e2e".into()).await;
}

// State B at the wiring level: no relay signal in the spawn env means
// zero injection even with non-empty server defaults wired in.
#[tokio::test]
async fn local_spawn_meta_stays_free_of_relay_keys() {
    let daemon = start_daemon();
    let _ = DaemonClient::new(&daemon.socket)
        .call(kallip_daemon_common::wire::RequestBody::List)
        .await;

    let backend = kallip_instances::backend::UdsBackend::arc(DaemonClient::new(&daemon.socket));
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let spawned = backend
        .spawn(
            "local-e2e".into(),
            workspace.path().display().to_string(),
            vec![
                "KALLIP_OPERATOR_TOKEN=test-op-token".into(),
                "KALLIP_LLM_PROVIDER=deepseek".into(),
                "KALLIP_LLM_MODEL=test-model".into(),
                "KALLIP_LLM_DEEPSEEK_API_KEY=test-key".into(),
            ],
            daemon_user(),
        )
        .await
        .expect("local spawn");
    assert_eq!(spawned.slug, "local-e2e");

    let record_path = daemon.records.join("local-e2e.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).expect("record"))
            .expect("parse record");
    let env = record["env"].as_array().expect("env array");
    assert!(
        env.iter().all(|e| !e
            .as_str()
            .unwrap_or_default()
            .starts_with("KALLIP_TAGMA_RELAY_")
            && !e
                .as_str()
                .unwrap_or_default()
                .starts_with("KALLIP_POLIS_URL=")),
        "no relay or origin keys injected: {env:?}"
    );

    let _ = backend.stop("local-e2e".into()).await;
}

// The wire contract for existing callers: a body with no user key
// still parses (serde default = None) and reaches the daemon, whose
// implicit-launch rules then decide. Under a root daemon that rule
// is the refused self-launch; a non-root host would accept and
// launch, so the assertion is root-only.

// A red against an older daemon binary is version discrimination
// (this test canaries the guard's presence), not an environment
// fault.
#[tokio::test]
async fn spawn_without_user_key_still_parses_and_hits_guard() {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let daemon = start_daemon();
    let _ = DaemonClient::new(&daemon.socket)
        .call(kallip_daemon_common::wire::RequestBody::List)
        .await;

    let state = AppState {
        backend: kallip_instances::backend::UdsBackend::arc(DaemonClient::new(&daemon.socket)),
        auth: kallip_instances::guard::AuthMode::Token("itest-token".into()),
        cors_origins: String::new(),
        allowed_hosts: vec![],
    };
    let app = build_router(state);

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let spawn_body = serde_json::json!({
        "slug": "no-user-key",
        "workspace": workspace.path().display().to_string(),
        "env": [
            "KALLIP_OPERATOR_TOKEN=test-op-token",
            "KALLIP_LLM_PROVIDER=deepseek",
            "KALLIP_LLM_MODEL=test-model",
            "KALLIP_LLM_DEEPSEEK_API_KEY=test-key",
        ],
    })
    .to_string();
    let (status, body) = send(
        &app,
        "POST",
        "/spawn",
        Some("itest-token"),
        Some(&spawn_body),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("\"invalid_spawn_input\""), "{body}");
}
/// A scriptable stand-in for the archeion verifier: each call consumes the next
/// programmed outcome.
struct MockVerifier {
    outcomes: std::sync::Mutex<
        Vec<
            Result<
                Option<kallip_archeion_common::principal::Principal>,
                kallip_archeion_common::control_plane::ControlPlaneError,
            >,
        >,
    >,
    /// Same scripting shape for the session-cookie channel; popped only
    /// when a request arrives without a bearer.
    session_outcomes: std::sync::Mutex<
        Vec<
            Result<
                Option<kallip_archeion_common::control_plane::VerifiedSession>,
                kallip_archeion_common::control_plane::ControlPlaneError,
            >,
        >,
    >,
}

#[async_trait::async_trait]
impl kallip_instances::control_plane::AuthVerifier for MockVerifier {
    async fn verify_bearer(
        &self,
        _token: &str,
    ) -> Result<
        Option<kallip_archeion_common::principal::Principal>,
        kallip_archeion_common::control_plane::ControlPlaneError,
    > {
        self.outcomes
            .lock()
            .unwrap()
            .pop()
            .expect("programmed outcome")
    }

    async fn verify_session(
        &self,
        _cookie: &str,
    ) -> Result<
        Option<kallip_archeion_common::control_plane::VerifiedSession>,
        kallip_archeion_common::control_plane::ControlPlaneError,
    > {
        // The session-channel tests script `session_outcomes`; an
        // unprogrammed pop means a bearer-path test unexpectedly took
        // the cookie branch.
        self.session_outcomes
            .lock()
            .unwrap()
            .pop()
            .expect("programmed session outcome")
    }
}

#[tokio::test]
async fn platform_mode_admin_only_and_fail_closed() {
    use kallip_archeion_common::control_plane::ControlPlaneError;
    use kallip_archeion_common::ids::TagmaId;
    use kallip_archeion_common::principal::Principal;
    use kallip_instances::guard::AuthMode;

    let verifier = std::sync::Arc::new(MockVerifier {
        session_outcomes: std::sync::Mutex::new(vec![]),
        outcomes: std::sync::Mutex::new(vec![
            // Last popped first: reverse program order.
            Err(ControlPlaneError::Backend("archeion down".into())),
            Ok(None),
            Ok(Some(Principal::User(
                kallip_archeion_common::ids::UserId::from("u1".to_string()),
            ))),
            Ok(Some(Principal::Tagma(TagmaId::from("t1".to_string())))),
            Ok(Some(Principal::Admin)),
        ]),
    });
    let state = AppState {
        backend: kallip_instances::backend::UdsBackend::arc(DaemonClient::new(
            "/nonexistent-kallip-test.sock",
        )),
        auth: AuthMode::Platform(verifier),
        allowed_hosts: vec![],
        cors_origins: String::new(),
    };
    let app = build_router(state);

    // Admin passes the gate (and dies at the daemon proxy: 503 proves the
    // guard let it through).
    let (status, _) = send(&app, "GET", "/list", Some("any"), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // A valid Tagma identity: 403.
    let (status, body) = send(&app, "GET", "/list", Some("any"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("\"forbidden\""), "{body}");

    // A valid User identity: 403.
    let (status, _) = send(&app, "GET", "/list", Some("any"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An invalid token: 401.
    let (status, body) = send(&app, "GET", "/list", Some("any"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Archeion unreachable: fail closed.
    let (status, body) = send(&app, "GET", "/list", Some("any"), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("\"auth_backend_unavailable\""), "{body}");
}

/// The browser channel: no bearer, the `kallip_session` cookie instead.
/// Only the local-admin account's session passes; a plain user session
/// is 403, an absent one 401.
#[tokio::test]
async fn platform_mode_session_channel_local_admin_only() {
    use kallip_archeion_common::control_plane::VerifiedSession;
    use kallip_archeion_common::ids::UserId;
    use kallip_instances::guard::AuthMode;

    let session = |local_admin| VerifiedSession {
        user_id: UserId::from("u1".to_string()),
        username: "admin".to_string(),
        display_name: None,
        local_admin,
    };
    let verifier = std::sync::Arc::new(MockVerifier {
        outcomes: std::sync::Mutex::new(vec![]),
        // Last popped first: reverse program order.
        session_outcomes: std::sync::Mutex::new(vec![
            Ok(None),
            Ok(Some(session(false))),
            Ok(Some(session(true))),
        ]),
    });
    let state = AppState {
        backend: kallip_instances::backend::UdsBackend::arc(DaemonClient::new(
            "/nonexistent-kallip-test.sock",
        )),
        auth: AuthMode::Platform(verifier),
        allowed_hosts: vec![],
        cors_origins: String::new(),
    };
    let app = build_router(state);

    // The local-admin session passes the gate (503 = the dead daemon
    // proxy answered, so the guard let it through).
    let response = app
        .clone()
        .oneshot(
            Request::get("/list")
                .header("host", "127.0.0.1:7300")
                .header("cookie", "kallip_session=sk-sess-x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // A plain user session: authenticated but not allowed.
    let response = app
        .clone()
        .oneshot(
            Request::get("/list")
                .header("host", "127.0.0.1:7300")
                .header("cookie", "kallip_session=sk-sess-x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // An absent/expired session: 401.
    let response = app
        .clone()
        .oneshot(
            Request::get("/list")
                .header("host", "127.0.0.1:7300")
                .header("cookie", "kallip_session=sk-sess-x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // The UI branches on this machine-readable code: without it a drift
    // would silently fall back to the standalone token form.
    let bytes = response.into_body().collect().await.expect("read");
    let body = String::from_utf8(bytes.to_bytes().to_vec()).expect("utf8");
    assert!(body.contains("admin_session_required"), "{body}");
}

/// The CSRF pillar for the cookie channel: a cookie-bearing mutating
/// request without the custom marker is 403 before it reaches auth; with
/// the marker it proceeds; a bearer request is exempt (the header is
/// itself proof of intent).
#[tokio::test]
async fn csrf_guard_cookie_channel() {
    use kallip_archeion_common::control_plane::VerifiedSession;
    use kallip_archeion_common::ids::UserId;
    use kallip_archeion_common::principal::Principal;
    use kallip_instances::guard::AuthMode;

    let verifier = std::sync::Arc::new(MockVerifier {
        outcomes: std::sync::Mutex::new(vec![Ok(Some(Principal::Admin))]),
        session_outcomes: std::sync::Mutex::new(vec![Ok(Some(VerifiedSession {
            user_id: UserId::from("u1".to_string()),
            username: "admin".to_string(),
            display_name: None,
            local_admin: true,
        }))]),
    });
    let state = AppState {
        backend: kallip_instances::backend::UdsBackend::arc(DaemonClient::new(
            "/nonexistent-kallip-test.sock",
        )),
        auth: AuthMode::Platform(verifier),
        allowed_hosts: vec![],
        cors_origins: String::new(),
    };
    let app = build_router(state);

    // 1) Cookie-bearing POST without the marker: 403 at the CSRF layer
    // (the plain-text body is the guard's, not an ApiFault).
    let response = app
        .clone()
        .oneshot(
            Request::post("/spawn")
                .header("host", "127.0.0.1:7300")
                .header("cookie", "kallip_session=sk-sess-x")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // 2) With the marker it proceeds through auth (503 = dead daemon).
    let response = app
        .clone()
        .oneshot(
            Request::post("/stop")
                .header("host", "127.0.0.1:7300")
                .header("cookie", "kallip_session=sk-sess-x")
                .header("x-requested-with", "kallip")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"slug":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // 3) Bearer exemption: no cookie, no marker, still proceeds.
    let response = app
        .clone()
        .oneshot(
            Request::post("/stop")
                .header("host", "127.0.0.1:7300")
                .header("authorization", "Bearer any")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"slug":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn refuses_to_start_unauthenticated_on_non_loopback() {
    // The fail-safe rule: non-loopback bind without either auth mode.
    let config = kallip_instances::Config {
        addr: "0.0.0.0:7300".into(),
        daemon_socket: None,
        token: None,
        backend: "daemon".into(),
        archeion_internal_url: None,
        archeion_internal_token: None,
        archeion_internal_token_file: None,
        allowed_hosts_raw: String::new(),
        cors_origins: String::new(),
    };
    let error = kallip_instances::resolve_auth(&config, &config.addr).expect_err("must refuse");
    assert!(error.to_string().contains("refusing to start"), "{error}");
}

#[test]
fn open_mode_allowed_on_loopback() {
    let config = kallip_instances::Config {
        addr: "127.0.0.1:7300".into(),
        daemon_socket: None,
        token: None,
        backend: "daemon".into(),
        archeion_internal_url: None,
        archeion_internal_token: None,
        archeion_internal_token_file: None,
        allowed_hosts_raw: String::new(),
        cors_origins: String::new(),
    };
    assert!(matches!(
        kallip_instances::resolve_auth(&config, &config.addr).expect("resolve"),
        kallip_instances::guard::AuthMode::Open
    ));
}
#[test]
fn half_configured_archeion_url_refuses_to_start() {
    // A URL without the internal token must not silently fall through
    // to open mode on a loopback bind.
    let config = kallip_instances::Config {
        addr: "127.0.0.1:7300".into(),
        daemon_socket: None,
        token: None,
        backend: "daemon".into(),
        archeion_internal_url: Some("http://127.0.0.1:7100".into()),
        archeion_internal_token: None,
        archeion_internal_token_file: None,
        allowed_hosts_raw: String::new(),
        cors_origins: String::new(),
    };
    let error = kallip_instances::resolve_auth(&config, &config.addr).expect_err("must refuse");
    assert!(
        error
            .to_string()
            .contains("KALLIP_POLIS_INTERNAL_TOKEN_FILE"),
        "{error}"
    );
}

#[test]
fn half_configured_archeion_token_refuses_to_start() {
    let config = kallip_instances::Config {
        addr: "127.0.0.1:7300".into(),
        daemon_socket: None,
        token: None,
        backend: "daemon".into(),
        archeion_internal_url: None,
        archeion_internal_token: Some("internal-secret".into()),
        archeion_internal_token_file: None,
        allowed_hosts_raw: String::new(),
        cors_origins: String::new(),
    };
    let error = kallip_instances::resolve_auth(&config, &config.addr).expect_err("must refuse");
    assert!(
        error.to_string().contains("KALLIP_INSTANCES_ARCHEION_URL"),
        "{error}"
    );
}
